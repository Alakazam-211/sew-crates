//! The media transport of one call: UDP host candidates and one TURN relay
//! candidate (RFC 8656) over UDP, TCP or TLS, behind one [`Transport`].
//!
//! str0m (the ICE agent, [`crate::pc`]) is Sans-IO: it emits datagrams with a source address
//! (one of [`Transport::local_candidates`]) and a destination; [`Transport::send`] routes each
//! to the socket or TURN allocation that owns the source, and [`Transport::recv`] yields
//! received datagrams addressed the same way.
//!
//! **Relay-only** (`media.relay_only` or `force_relay`): only the TURN relay candidate, through
//! the `turns:` server; with no `turns:` server on offer the transport refuses
//! ([`TransportError::RelayOnlyNoTurns`]) and no media is sent. Otherwise: host candidates on
//! the interface toward the TURN server, plus one relay (UDP, else TCP, else TLS). Server
//! reflexive candidates are not gathered (a relay covers the NAT case).
//!
//! `turn.rs` and `stun.rs` are the test client's TURN client and STUN codec
//! (Sew's media test client, not published; Alakazam Labs' own code, written from the RFCs).

pub(crate) mod stun;
pub(crate) mod turn;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sew_lkproto::livekit::IceServer;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::ClientConfig;
use turn::{TurnClient, TurnEvent};

/// How long a TURN allocation may take.
const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the TCP connect to a TURN server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Which candidates to gather.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gather {
    /// The TURN relay candidate only, through `turns:`.
    RelayOnly,
    /// Host candidates and a relay, as a browser does.
    All,
}

/// Why the transport could not open.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// Relay-only with no `turns:` server on offer (`relay_only_no_turns`).
    #[error("relay-only, but the engine offered no turns: server")]
    RelayOnlyNoTurns,
    /// No TURN server could be reached (`turn_unreachable`).
    #[error("TURN unreachable: {0}")]
    TurnUnreachable(String),
    /// A socket error.
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// A parsed ICE server URL (RFC 7064/7065).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IceUrl {
    /// `stun`, `turn` or `turns`.
    pub(crate) scheme: String,
    /// Host (name or address).
    pub(crate) host: String,
    /// Port.
    pub(crate) port: u16,
    /// `udp` or `tcp` (`turns` is always `tcp`).
    pub(crate) transport: String,
}

impl IceUrl {
    /// Parses `turn:host:port?transport=udp` and friends.
    pub(crate) fn parse(url: &str) -> Option<IceUrl> {
        let (scheme, rest) = url.split_once(':')?;
        let (hostport, query) = match rest.split_once('?') {
            Some((h, q)) => (h, Some(q)),
            None => (rest, None),
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (ip, p) = h.split_once(']')?;
            (
                ip.to_owned(),
                p.strip_prefix(':').and_then(|p| p.parse().ok()),
            )
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), p.parse().ok()),
                None => (hostport.to_owned(), None),
            }
        };
        let scheme = scheme.to_ascii_lowercase();
        let default_port = if scheme == "turns" { 5349 } else { 3478 };
        let mut transport = if scheme == "turns" { "tcp" } else { "udp" }.to_owned();
        if let Some(q) = query {
            for kv in q.split('&') {
                if let Some(v) = kv.strip_prefix("transport=") {
                    transport = v.to_ascii_lowercase();
                }
            }
        }
        if host.is_empty() {
            return None;
        }
        Some(IceUrl {
            scheme,
            host,
            port: port.unwrap_or(default_port),
            transport,
        })
    }
}

/// How the client reaches the TURN server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// TURN over UDP.
    Udp,
    /// TURN over TCP.
    Tcp,
    /// TURN over TLS (`turns:`).
    Tls,
}

impl Via {
    /// The name in snapshots and stats.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Via::Udp => "udp",
            Via::Tcp => "tcp",
            Via::Tls => "tls",
        }
    }
}

/// The TURN servers of a JoinResponse in the order to try for `gather`.
pub(crate) fn pick_turn(servers: &[IceServer], gather: Gather) -> Vec<(IceUrl, Via, IceServer)> {
    let mut out: Vec<(u8, IceUrl, Via, IceServer)> = Vec::new();
    for s in servers {
        for u in &s.urls {
            let Some(p) = IceUrl::parse(u) else { continue };
            let pick = match (gather, p.scheme.as_str(), p.transport.as_str()) {
                (Gather::RelayOnly, "turns", _) => Some((0, Via::Tls)),
                (Gather::RelayOnly, _, _) => None,
                (Gather::All, "turn", "udp") => Some((0, Via::Udp)),
                (Gather::All, "turn", "tcp") => Some((1, Via::Tcp)),
                (Gather::All, "turns", _) => Some((2, Via::Tls)),
                _ => None,
            };
            if let Some((rank, via)) = pick {
                out.push((rank, p, via, s.clone()));
            }
        }
    }
    out.sort_by_key(|c| c.0);
    out.into_iter().map(|(_, u, v, s)| (u, v, s)).collect()
}

/// The kind of a local candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LocalKind {
    /// A UDP host candidate.
    Host,
    /// The TURN relay candidate.
    Relayed {
        /// The TURN server.
        server: SocketAddr,
        /// The client↔TURN transport.
        via: Via,
        /// The local socket address toward the TURN server.
        base: SocketAddr,
    },
}

/// A local candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalCandidate {
    /// The candidate address (relayed: the relayed address).
    pub(crate) addr: SocketAddr,
    /// Its kind.
    pub(crate) kind: LocalKind,
}

/// A datagram at the ICE level (always UDP semantics: the relayed transport is UDP).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Datagram {
    /// Sender.
    pub(crate) source: SocketAddr,
    /// Receiver.
    pub(crate) destination: SocketAddr,
    /// Bytes.
    pub(crate) data: Vec<u8>,
}

/// What the transport reports.
#[derive(Debug)]
pub(crate) enum TransportEvent {
    /// A datagram arrived.
    Datagram(Datagram),
    /// The TURN allocation is gone (the TLS/TCP stream closed, a refresh failed): a full
    /// reconnect gets a new one with fresh credentials.
    TurnLost(String),
}

enum TurnCmd {
    Send(SocketAddr, Vec<u8>),
    Close,
}

/// The sockets and the TURN allocation of the call's one peer connection.
pub(crate) struct Transport {
    locals: Vec<LocalCandidate>,
    udp: HashMap<SocketAddr, Arc<UdpSocket>>,
    turn: HashMap<SocketAddr, mpsc::UnboundedSender<TurnCmd>>,
    in_rx: mpsc::UnboundedReceiver<TransportEvent>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("locals", &self.locals)
            .finish_non_exhaustive()
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        for tx in self.turn.values() {
            let _ = tx.send(TurnCmd::Close);
        }
        // The TURN tasks get a moment to send their deallocation; the readers stop now.
        for t in &self.tasks {
            t.abort();
        }
    }
}

type Ready = oneshot::Sender<Result<SocketAddr, String>>;

impl Transport {
    /// Gathers for `gather` with the JoinResponse's `servers`.
    pub(crate) async fn open(
        cfg: &ClientConfig,
        tls: &Arc<rustls::ClientConfig>,
        servers: &[IceServer],
        gather: Gather,
    ) -> Result<Transport, TransportError> {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let mut t = Transport {
            locals: Vec::new(),
            udp: HashMap::new(),
            turn: HashMap::new(),
            in_rx,
            tasks: Vec::new(),
        };
        let turns = pick_turn(servers, gather);
        if gather == Gather::RelayOnly && turns.is_empty() {
            return Err(TransportError::RelayOnlyNoTurns);
        }
        let mut last_err = None;
        let mut toward: Option<SocketAddr> = None;
        for (url, via, server) in &turns {
            let addr = match crate::config::resolve(cfg, &url.host, url.port).await {
                Ok(a) => a,
                Err(e) => {
                    last_err = Some(format!("resolve {}: {e}", url.host));
                    continue;
                }
            };
            toward.get_or_insert(addr);
            let source = crate::config::source_ip(cfg);
            match t
                .allocate(*via, addr, &url.host, server, tls, &in_tx, source)
                .await
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    tracing::info!(target: "sew_call_client", via = via.name(), error = %e, "TURN allocation failed");
                    last_err = Some(e);
                }
            }
        }
        if gather == Gather::RelayOnly && t.turn.is_empty() {
            return Err(TransportError::TurnUnreachable(
                last_err.unwrap_or_else(|| "no TURN server".to_owned()),
            ));
        }
        if gather == Gather::All {
            let ip = match crate::config::source_ip(cfg) {
                Some(ip) => ip,
                None => local_ip_toward(toward).await,
            };
            let s = Arc::new(UdpSocket::bind(SocketAddr::new(ip, 0)).await?);
            let addr = s.local_addr()?;
            t.spawn_udp_reader(s.clone(), addr, in_tx.clone());
            t.udp.insert(addr, s);
            t.locals.push(LocalCandidate {
                addr,
                kind: LocalKind::Host,
            });
        }
        Ok(t)
    }

    fn spawn_udp_reader(
        &mut self,
        s: Arc<UdpSocket>,
        local: SocketAddr,
        tx: mpsc::UnboundedSender<TransportEvent>,
    ) {
        self.tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                match s.recv_from(&mut buf).await {
                    Ok((n, from)) => {
                        let d = Datagram {
                            source: from,
                            destination: local,
                            data: buf[..n].to_vec(),
                        };
                        if tx.send(TransportEvent::Datagram(d)).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                    Err(_) => return,
                }
            }
        }));
    }

    #[allow(clippy::too_many_arguments)]
    async fn allocate(
        &mut self,
        via: Via,
        server_addr: SocketAddr,
        host: &str,
        server: &IceServer,
        tls: &Arc<rustls::ClientConfig>,
        in_tx: &mpsc::UnboundedSender<TransportEvent>,
        source: Option<IpAddr>,
    ) -> Result<(), String> {
        let mut client = TurnClient::new(&server.username, &server.credential, via != Via::Udp);
        client.start(Instant::now());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let in_tx = in_tx.clone();
        let (base, task) = match via {
            Via::Udp => {
                let bind = match source {
                    Some(ip) => SocketAddr::new(ip, 0),
                    None if server_addr.is_ipv4() => SocketAddr::from(([0, 0, 0, 0], 0)),
                    None => SocketAddr::from(([0u16; 8], 0)),
                };
                let s = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
                s.connect(server_addr).await.map_err(|e| e.to_string())?;
                let base = s.local_addr().map_err(|e| e.to_string())?;
                (
                    base,
                    tokio::spawn(turn_udp_task(s, client, cmd_rx, in_tx, ready_tx)),
                )
            }
            Via::Tcp | Via::Tls => {
                let tcp = tokio::time::timeout(CONNECT_TIMEOUT, tcp_connect(server_addr, source))
                    .await
                    .map_err(|_| format!("TCP connect to {server_addr} timed out"))?
                    .map_err(|e| format!("TCP connect to {server_addr}: {e}"))?;
                let _ = tcp.set_nodelay(true);
                let base = tcp.local_addr().map_err(|e| e.to_string())?;
                if via == Via::Tls {
                    let name = rustls::pki_types::ServerName::try_from(
                        host.trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_owned(),
                    )
                    .map_err(|e| e.to_string())?;
                    let conn = tokio_rustls::TlsConnector::from(tls.clone());
                    let s = tokio::time::timeout(CONNECT_TIMEOUT, conn.connect(name, tcp))
                        .await
                        .map_err(|_| "TLS handshake timed out".to_owned())?
                        .map_err(|e| format!("TLS: {e}"))?;
                    (
                        base,
                        tokio::spawn(turn_stream_task(s, client, cmd_rx, in_tx, ready_tx)),
                    )
                } else {
                    (
                        base,
                        tokio::spawn(turn_stream_task(tcp, client, cmd_rx, in_tx, ready_tx)),
                    )
                }
            }
        };
        self.tasks.push(task);
        let relayed = tokio::time::timeout(ALLOCATE_TIMEOUT, ready_rx)
            .await
            .map_err(|_| format!("TURN allocation at {server_addr} timed out"))?
            .map_err(|_| "TURN task ended".to_owned())??;
        self.turn.insert(relayed, cmd_tx);
        self.locals.push(LocalCandidate {
            addr: relayed,
            kind: LocalKind::Relayed {
                server: server_addr,
                via,
                base,
            },
        });
        Ok(())
    }

    /// The local candidates for the ICE agent.
    pub(crate) fn local_candidates(&self) -> &[LocalCandidate] {
        &self.locals
    }

    /// The relay's transport, if one was allocated.
    pub(crate) fn relay_via(&self) -> Option<Via> {
        self.locals.iter().find_map(|l| match l.kind {
            LocalKind::Relayed { via, .. } => Some(via),
            LocalKind::Host => None,
        })
    }

    /// Sends a datagram the ICE agent produced, by its source.
    pub(crate) fn send(&mut self, d: Datagram) {
        if let Some(s) = self.udp.get(&d.source) {
            let _ = s.try_send_to(&d.data, d.destination);
        } else if let Some(tx) = self.turn.get(&d.source) {
            let _ = tx.send(TurnCmd::Send(d.destination, d.data));
        }
    }

    /// The next event.
    pub(crate) async fn recv(&mut self) -> Option<TransportEvent> {
        self.in_rx.recv().await
    }
}

/// A TCP connection to `to`, from `source` when one is set (else the OS's choice).
async fn tcp_connect(to: SocketAddr, source: Option<IpAddr>) -> io::Result<TcpStream> {
    let Some(ip) = source else {
        return TcpStream::connect(to).await;
    };
    let s = if ip.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    s.bind(SocketAddr::new(ip, 0))?;
    s.connect(to).await
}

/// The local address the OS would use toward `toward` (no packet is sent), else loopback.
async fn local_ip_toward(toward: Option<SocketAddr>) -> IpAddr {
    let fallback = IpAddr::from([127, 0, 0, 1]);
    let Some(to) = toward else { return fallback };
    let bind = if to.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let Ok(s) = UdpSocket::bind(bind).await else {
        return fallback;
    };
    if s.connect(to).await.is_err() {
        return fallback;
    }
    s.local_addr().map(|a| a.ip()).unwrap_or(fallback)
}

/// Drains the TURN client's events; `false` when the allocation is gone.
fn drain_turn(
    client: &mut TurnClient,
    in_tx: &mpsc::UnboundedSender<TransportEvent>,
    ready: &mut Option<Ready>,
) -> bool {
    while let Some(ev) = client.poll_event() {
        match ev {
            TurnEvent::Allocated { relayed, .. } => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Ok(relayed));
                }
            }
            TurnEvent::Data { peer, data } => {
                if let Some(relayed) = client.relayed() {
                    let d = Datagram {
                        source: peer,
                        destination: relayed,
                        data,
                    };
                    let _ = in_tx.send(TransportEvent::Datagram(d));
                }
            }
            TurnEvent::Failed {
                method,
                code,
                reason,
            } => {
                let e = format!("TURN {method:?} failed: {code} {reason}");
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(e));
                    return false;
                }
                if matches!(method, stun::Method::Allocate | stun::Method::Refresh) {
                    let _ = in_tx.send(TransportEvent::TurnLost(e));
                    return false;
                }
                tracing::debug!(target: "sew_call_client", "{e}");
            }
            TurnEvent::ChannelBound { .. } => {}
        }
    }
    true
}

fn sleep_until_opt(t: Option<Instant>) -> tokio::time::Sleep {
    let at = t.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
    tokio::time::sleep_until(tokio::time::Instant::from_std(at))
}

async fn turn_udp_task(
    s: UdpSocket,
    mut client: TurnClient,
    mut cmd_rx: mpsc::UnboundedReceiver<TurnCmd>,
    in_tx: mpsc::UnboundedSender<TransportEvent>,
    ready: Ready,
) {
    let mut ready = Some(ready);
    let mut buf = vec![0u8; 65_536];
    loop {
        while let Some(b) = client.poll_transmit() {
            let _ = s.send(&b).await;
        }
        if !drain_turn(&mut client, &in_tx, &mut ready) {
            return;
        }
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(TurnCmd::Send(peer, data)) => client.send_to(peer, &data, Instant::now()),
                Some(TurnCmd::Close) | None => {
                    client.close(Instant::now());
                    while let Some(b) = client.poll_transmit() {
                        let _ = s.send(&b).await;
                    }
                    return;
                }
            },
            r = s.recv(&mut buf) => match r {
                Ok(n) => client.handle_input(&buf[..n], Instant::now()),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
                Err(e) => {
                    let _ = in_tx.send(TransportEvent::TurnLost(format!("TURN UDP: {e}")));
                    return;
                }
            },
            () = sleep_until_opt(client.poll_timeout()) => client.handle_timeout(Instant::now()),
        }
    }
}

async fn turn_stream_task<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    mut client: TurnClient,
    mut cmd_rx: mpsc::UnboundedReceiver<TurnCmd>,
    in_tx: mpsc::UnboundedSender<TransportEvent>,
    ready: Ready,
) {
    let mut ready = Some(ready);
    let (mut r, mut w) = tokio::io::split(stream);
    let mut framer = stun::StreamFramer::default();
    let mut buf = vec![0u8; 65_536];
    let lost = |ready: &mut Option<Ready>, why: &str| {
        match ready.take() {
            Some(rd) => {
                let _ = rd.send(Err(why.to_owned()));
            }
            None => {
                let _ = in_tx.send(TransportEvent::TurnLost(why.to_owned()));
            }
        };
    };
    loop {
        while let Some(b) = client.poll_transmit() {
            if w.write_all(&b).await.is_err() {
                lost(&mut ready, "TURN stream write failed");
                return;
            }
        }
        if !drain_turn(&mut client, &in_tx, &mut ready) {
            return;
        }
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(TurnCmd::Send(peer, data)) => client.send_to(peer, &data, Instant::now()),
                Some(TurnCmd::Close) | None => {
                    client.close(Instant::now());
                    while let Some(b) = client.poll_transmit() {
                        let _ = w.write_all(&b).await;
                    }
                    let _ = w.shutdown().await;
                    return;
                }
            },
            n = r.read(&mut buf) => match n {
                Ok(0) | Err(_) => {
                    lost(&mut ready, "TURN stream closed");
                    return;
                }
                Ok(n) => {
                    framer.push(&buf[..n]);
                    while let Some(m) = framer.next_message() {
                        client.handle_input(&m, Instant::now());
                    }
                }
            },
            () = sleep_until_opt(client.poll_timeout()) => client.handle_timeout(Instant::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn server(urls: &[&str]) -> IceServer {
        IceServer {
            urls: urls.iter().map(|u| (*u).to_owned()).collect(),
            username: "u".to_owned(),
            credential: "c".to_owned(),
        }
    }

    #[test]
    fn ice_urls_parse() {
        let u = IceUrl::parse("turn:127.0.0.1:3478?transport=udp").unwrap();
        assert_eq!(
            (u.scheme.as_str(), u.port, u.transport.as_str()),
            ("turn", 3478, "udp")
        );
        let u = IceUrl::parse("turns:turn.sew.test:443?transport=tcp").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("turn.sew.test", 443));
        let u = IceUrl::parse("turn:[::1]:4000").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 4000));
        assert_eq!(IceUrl::parse("turns:turn.sew.test").unwrap().port, 5349);
        assert!(IceUrl::parse("turn:").is_none());
    }

    #[test]
    fn relay_only_picks_turns_only() {
        let s = [server(&[
            "turn:t.example:3478?transport=udp",
            "turn:t.example:3478?transport=tcp",
            "turns:t.example:443?transport=tcp",
        ])];
        let r = pick_turn(&s, Gather::RelayOnly);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, Via::Tls);
        let all = pick_turn(&s, Gather::All);
        assert_eq!(
            all.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![Via::Udp, Via::Tcp, Via::Tls]
        );
        let no_turns = [server(&["turn:t.example:3478?transport=udp"])];
        assert!(pick_turn(&no_turns, Gather::RelayOnly).is_empty());
    }

    #[tokio::test]
    async fn relay_only_without_turns_refuses() {
        // ring passed in, so this runs without the built-in provider too (`--no-default-features`).
        let cfg = ClientConfig::new("t", std::env::temp_dir())
            .with_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()));
        let tls = crate::config::tls_config(&cfg).unwrap();
        let s = [server(&[
            "turn:127.0.0.1:3478?transport=udp",
            "stun:127.0.0.1:3478",
        ])];
        let r = Transport::open(&cfg, &tls, &s, Gather::RelayOnly).await;
        assert!(matches!(r, Err(TransportError::RelayOnlyNoTurns)), "{r:?}");
    }
}
