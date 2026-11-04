//! (Copied from Sew's media test client, not published: Alakazam Labs' own code.)
//!
//! A Sans-IO TURN client (RFC 8656), written from the RFC: Allocate with long-term
//! credentials (the 401 → realm/nonce retry, 438 stale-nonce retry), Refresh at half the
//! lifetime, ChannelBind per peer (which also installs the permission), ChannelData for
//! bound peers and Send indications (after CreatePermission) while a bind is pending, Data
//! indications and ChannelData on receive. Retransmits requests over UDP (RFC 8489 §6.2.1).
//!
//! The caller owns the socket or stream to the TURN server: it feeds received bytes to
//! [`TurnClient::handle_input`], sends [`TurnClient::poll_transmit`] bytes, and calls
//! [`TurnClient::handle_timeout`] at [`TurnClient::poll_timeout`].

// Not every codec item is used by the engine (the test client uses more of it).
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::stun::{
    Class, Message, Method, attr, decode_channel_data, encode_channel_data, encode_xor_addr,
    long_term_key,
};

/// What the TURN client reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnEvent {
    /// The allocation succeeded.
    Allocated {
        /// XOR-RELAYED-ADDRESS: the relayed transport address (the relay candidate).
        relayed: SocketAddr,
        /// XOR-MAPPED-ADDRESS: the client's address as the server saw it.
        mapped: Option<SocketAddr>,
        /// The granted lifetime.
        lifetime: Duration,
    },
    /// Data from a peer through the relay.
    Data {
        /// The peer.
        peer: SocketAddr,
        /// The payload.
        data: Vec<u8>,
    },
    /// A request failed for good.
    Failed {
        /// The method.
        method: Method,
        /// The error code, if any (0 = timeout).
        code: u16,
        /// The reason phrase.
        reason: String,
    },
    /// A ChannelBind succeeded.
    ChannelBound {
        /// The peer.
        peer: SocketAddr,
        /// The channel number.
        channel: u16,
    },
}

#[derive(Clone, Debug)]
struct Pending {
    msg: Message,
    sent: Instant,
    rto: Duration,
    tries: u32,
    peer: Option<SocketAddr>,
}

/// The counters a test can read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TurnStats {
    /// Allocate requests sent (including the unauthenticated first one).
    pub(crate) allocate_requests: u64,
    /// Refresh requests sent.
    pub(crate) refreshes: u64,
    /// ChannelBind requests sent.
    pub(crate) channel_binds: u64,
    /// CreatePermission requests sent.
    pub(crate) create_permissions: u64,
    /// Payloads sent as ChannelData.
    pub(crate) channel_data_sent: u64,
    /// Payloads sent as Send indications.
    pub(crate) send_indications: u64,
    /// Payloads received from peers.
    pub(crate) data_received: u64,
}

/// The TURN client state machine.
#[derive(Debug)]
pub(crate) struct TurnClient {
    username: String,
    password: String,
    realm: Option<String>,
    nonce: Option<String>,
    key: Option<[u8; 16]>,
    stream: bool,
    relayed: Option<SocketAddr>,
    lifetime: Duration,
    refresh_at: Option<Instant>,
    pending: HashMap<[u8; 12], Pending>,
    channels: HashMap<SocketAddr, u16>,
    channel_peers: HashMap<u16, SocketAddr>,
    bound: HashMap<SocketAddr, Instant>,
    permitted: HashMap<std::net::IpAddr, Instant>,
    next_channel: u16,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<TurnEvent>,
    stats: TurnStats,
    closed: bool,
}

const RTO: Duration = Duration::from_millis(500);
const MAX_TRIES: u32 = 7;
/// Channel bindings last 10 min; refresh at 5 (RFC 8656 §12).
const CHANNEL_REFRESH: Duration = Duration::from_secs(300);
/// Permissions last 5 min; refresh at 4 (RFC 8656 §9).
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);

impl TurnClient {
    /// A client with the long-term credentials from the JoinResponse ICE server. `stream` is
    /// true over TCP/TLS (no retransmits; ChannelData padded).
    pub(crate) fn new(username: &str, password: &str, stream: bool) -> TurnClient {
        TurnClient {
            username: username.to_owned(),
            password: password.to_owned(),
            realm: None,
            nonce: None,
            key: None,
            stream,
            relayed: None,
            lifetime: Duration::from_secs(600),
            refresh_at: None,
            pending: HashMap::new(),
            channels: HashMap::new(),
            channel_peers: HashMap::new(),
            bound: HashMap::new(),
            permitted: HashMap::new(),
            next_channel: 0x4000,
            out: VecDeque::new(),
            events: VecDeque::new(),
            stats: TurnStats::default(),
            closed: false,
        }
    }

    /// The relayed address once allocated.
    pub(crate) fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// The counters.
    pub(crate) fn stats(&self) -> TurnStats {
        self.stats
    }

    /// Every peer the client asked the server to permit and bind (CreatePermission and
    /// ChannelBind targets), in order.
    pub(crate) fn peers(&self) -> Vec<SocketAddr> {
        let mut v: Vec<(u16, SocketAddr)> = self.channels.iter().map(|(p, c)| (*c, *p)).collect();
        v.sort();
        v.into_iter().map(|(_, p)| p).collect()
    }

    /// Starts the allocation (an unauthenticated Allocate that draws the 401 challenge).
    pub(crate) fn start(&mut self, now: Instant) {
        self.stats.allocate_requests += 1;
        let m = Message::new(Method::Allocate, Class::Request)
            .with(attr::REQUESTED_TRANSPORT, vec![17, 0, 0, 0]);
        self.send_request(m, None, now);
    }

    fn authed(&self, mut m: Message) -> Message {
        m.attrs
            .push((attr::USERNAME, self.username.as_bytes().to_vec()));
        if let Some(r) = &self.realm {
            m.attrs.push((attr::REALM, r.as_bytes().to_vec()));
        }
        if let Some(n) = &self.nonce {
            m.attrs.push((attr::NONCE, n.as_bytes().to_vec()));
        }
        m
    }

    fn send_request(&mut self, msg: Message, peer: Option<SocketAddr>, now: Instant) {
        let bytes = msg.encode(self.key.as_ref().map(|k| &k[..]));
        self.out.push_back(bytes);
        self.pending.insert(
            msg.tid,
            Pending {
                msg,
                sent: now,
                rto: RTO,
                tries: 1,
                peer,
            },
        );
    }

    fn refresh(&mut self, lifetime: u32, now: Instant) {
        self.stats.refreshes += 1;
        let m = self.authed(
            Message::new(Method::Refresh, Class::Request)
                .with(attr::LIFETIME, lifetime.to_be_bytes().to_vec()),
        );
        self.send_request(m, None, now);
    }

    fn channel_bind(&mut self, peer: SocketAddr, channel: u16, now: Instant) {
        self.stats.channel_binds += 1;
        let m = Message::new(Method::ChannelBind, Class::Request);
        let tid = m.tid;
        let m = self.authed(
            m.with(
                attr::CHANNEL_NUMBER,
                [channel.to_be_bytes(), [0, 0]].concat(),
            )
            .with(attr::XOR_PEER_ADDRESS, encode_xor_addr(peer, &tid)),
        );
        self.send_request(m, Some(peer), now);
    }

    fn create_permission(&mut self, peer: SocketAddr, now: Instant) {
        self.stats.create_permissions += 1;
        let m = Message::new(Method::CreatePermission, Class::Request);
        let tid = m.tid;
        let m = self.authed(m.with(attr::XOR_PEER_ADDRESS, encode_xor_addr(peer, &tid)));
        self.send_request(m, Some(peer), now);
    }

    /// Sends `data` to `peer` through the relay: ChannelData once the channel is bound, else
    /// a Send indication (after the first bind request, which also creates the permission).
    pub(crate) fn send_to(&mut self, peer: SocketAddr, data: &[u8], now: Instant) {
        if self.closed || self.relayed.is_none() {
            return;
        }
        if let Some(ch) = self.channels.get(&peer).copied() {
            if self.bound.contains_key(&peer) {
                self.stats.channel_data_sent += 1;
                self.out
                    .push_back(encode_channel_data(ch, data, self.stream));
                return;
            }
        } else {
            let ch = self.next_channel;
            self.next_channel = self.next_channel.wrapping_add(1).max(0x4000);
            self.channels.insert(peer, ch);
            self.channel_peers.insert(ch, peer);
            self.create_permission(peer, now);
            self.channel_bind(peer, ch, now);
        }
        // Not bound yet: a Send indication (needs the permission; the server drops it until
        // CreatePermission succeeds, which ICE's retransmits absorb).
        self.stats.send_indications += 1;
        let m = Message::new(Method::Send, Class::Indication);
        let tid = m.tid;
        let m = m
            .with(attr::XOR_PEER_ADDRESS, encode_xor_addr(peer, &tid))
            .with(attr::DATA, data.to_vec());
        self.out.push_back(m.encode(None));
    }

    /// Feeds bytes received from the TURN server (one whole message).
    pub(crate) fn handle_input(&mut self, b: &[u8], now: Instant) {
        if let Some((ch, data)) = decode_channel_data(b) {
            if let Some(peer) = self.channel_peers.get(&ch).copied() {
                self.stats.data_received += 1;
                self.events.push_back(TurnEvent::Data {
                    peer,
                    data: data.to_vec(),
                });
            }
            return;
        }
        let Ok(m) = Message::decode(b) else {
            return;
        };
        if m.method == Method::Data && m.class == Class::Indication {
            if let (Some(peer), Some(data)) =
                (m.xor_addr(attr::XOR_PEER_ADDRESS), m.get(attr::DATA))
            {
                self.stats.data_received += 1;
                self.events.push_back(TurnEvent::Data {
                    peer,
                    data: data.to_vec(),
                });
            }
            return;
        }
        let Some(p) = self.pending.remove(&m.tid) else {
            return;
        };
        match m.class {
            Class::Success => self.on_success(&p, &m, now),
            Class::Error => self.on_error(p, &m, now),
            _ => {}
        }
    }

    fn on_success(&mut self, p: &Pending, m: &Message, now: Instant) {
        match p.msg.method {
            Method::Allocate => {
                let lifetime = m
                    .get(attr::LIFETIME)
                    .filter(|v| v.len() == 4)
                    .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
                    .unwrap_or(600);
                self.lifetime = Duration::from_secs(u64::from(lifetime));
                self.refresh_at = Some(now + self.lifetime / 2);
                if let Some(relayed) = m.xor_addr(attr::XOR_RELAYED_ADDRESS) {
                    self.relayed = Some(relayed);
                    self.events.push_back(TurnEvent::Allocated {
                        relayed,
                        mapped: m.xor_addr(attr::XOR_MAPPED_ADDRESS),
                        lifetime: self.lifetime,
                    });
                }
            }
            Method::Refresh => {
                if let Some(v) = m.get(attr::LIFETIME).filter(|v| v.len() == 4) {
                    self.lifetime = Duration::from_secs(u64::from(u32::from_be_bytes([
                        v[0], v[1], v[2], v[3],
                    ])));
                }
                self.refresh_at = Some(now + self.lifetime / 2);
            }
            Method::ChannelBind => {
                if let Some(peer) = p.peer {
                    self.bound.insert(peer, now);
                    self.permitted.insert(peer.ip(), now);
                    if let Some(ch) = self.channels.get(&peer).copied() {
                        self.events
                            .push_back(TurnEvent::ChannelBound { peer, channel: ch });
                    }
                }
            }
            Method::CreatePermission => {
                if let Some(peer) = p.peer {
                    self.permitted.insert(peer.ip(), now);
                }
            }
            _ => {}
        }
    }

    fn on_error(&mut self, p: Pending, m: &Message, now: Instant) {
        let (code, reason) = m.error_code().unwrap_or((0, String::new()));
        let retry_auth =
            (code == 401 && p.msg.method == Method::Allocate && self.key.is_none()) || code == 438;
        if retry_auth {
            if let Some(r) = m.text(attr::REALM) {
                self.realm = Some(r);
            }
            if let Some(n) = m.text(attr::NONCE) {
                self.nonce = Some(n);
            }
            let realm = self.realm.clone().unwrap_or_default();
            self.key = Some(long_term_key(&self.username, &realm, &self.password));
            // Resend the same request with credentials and a new transaction id.
            let mut msg = Message::new(p.msg.method, Class::Request);
            msg.attrs = p
                .msg
                .attrs
                .iter()
                .filter(|(t, _)| ![attr::USERNAME, attr::REALM, attr::NONCE].contains(t))
                .cloned()
                .collect();
            // XOR attributes depend on the transaction id: re-encode them.
            if let Some(peer) = p.peer {
                for (t, v) in &mut msg.attrs {
                    if *t == attr::XOR_PEER_ADDRESS {
                        *v = encode_xor_addr(peer, &msg.tid);
                    }
                }
            }
            if p.msg.method == Method::Allocate {
                self.stats.allocate_requests += 1;
            }
            let msg = self.authed(msg);
            self.send_request(msg, p.peer, now);
            return;
        }
        self.events.push_back(TurnEvent::Failed {
            method: p.msg.method,
            code,
            reason,
        });
    }

    /// The next bytes to send to the TURN server.
    pub(crate) fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// The next event.
    pub(crate) fn poll_event(&mut self) -> Option<TurnEvent> {
        self.events.pop_front()
    }

    /// When [`TurnClient::handle_timeout`] must run next.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        let retrans = if self.stream {
            None
        } else {
            self.pending.values().map(|p| p.sent + p.rto).min()
        };
        let chan = self.bound.values().map(|t| *t + CHANNEL_REFRESH).min();
        let perm = self
            .permitted
            .values()
            .map(|t| *t + PERMISSION_REFRESH)
            .min();
        [retrans, self.refresh_at, chan, perm]
            .into_iter()
            .flatten()
            .min()
    }

    /// Retransmits, refreshes the allocation, bindings and permissions.
    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        if self.closed {
            return;
        }
        if !self.stream {
            let due: Vec<[u8; 12]> = self
                .pending
                .iter()
                .filter(|(_, p)| p.sent + p.rto <= now)
                .map(|(k, _)| *k)
                .collect();
            for tid in due {
                if let Some(mut p) = self.pending.remove(&tid) {
                    if p.tries >= MAX_TRIES {
                        self.events.push_back(TurnEvent::Failed {
                            method: p.msg.method,
                            code: 0,
                            reason: "timeout".to_owned(),
                        });
                        continue;
                    }
                    p.tries += 1;
                    p.sent = now;
                    p.rto = (p.rto * 2).min(Duration::from_secs(8));
                    self.out
                        .push_back(p.msg.encode(self.key.as_ref().map(|k| &k[..])));
                    self.pending.insert(tid, p);
                }
            }
        }
        if self.refresh_at.is_some_and(|t| t <= now) {
            self.refresh_at = Some(now + self.lifetime / 2);
            let secs = u32::try_from(self.lifetime.as_secs()).unwrap_or(600);
            self.refresh(secs, now);
        }
        let rebinds: Vec<SocketAddr> = self
            .bound
            .iter()
            .filter(|(_, t)| **t + CHANNEL_REFRESH <= now)
            .map(|(p, _)| *p)
            .collect();
        for peer in rebinds {
            self.bound.insert(peer, now);
            if let Some(ch) = self.channels.get(&peer).copied() {
                self.channel_bind(peer, ch, now);
            }
        }
        let perms: Vec<std::net::IpAddr> = self
            .permitted
            .iter()
            .filter(|(_, t)| **t + PERMISSION_REFRESH <= now)
            .map(|(ip, _)| *ip)
            .collect();
        for ip in perms {
            self.permitted.insert(ip, now);
            if let Some(peer) = self.channels.keys().find(|p| p.ip() == ip).copied() {
                self.create_permission(peer, now);
            }
        }
    }

    /// Deallocates (Refresh with lifetime 0) and stops.
    pub(crate) fn close(&mut self, now: Instant) {
        if self.relayed.is_some() && !self.closed {
            self.refresh(0, now);
        }
        self.closed = true;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::super::stun::verify_integrity;
    use super::*;

    fn reply(req: &[u8], class: Class, attrs: Vec<(u16, Vec<u8>)>, key: Option<&[u8]>) -> Vec<u8> {
        let r = Message::decode(req).unwrap();
        let mut m = Message::new(r.method, class);
        m.tid = r.tid;
        m.attrs = attrs;
        m.encode(key)
    }

    #[test]
    fn allocate_challenge_then_bind_and_channel_data() {
        let now = Instant::now();
        let mut c = TurnClient::new("user", "pass", false);
        c.start(now);
        let first = c.poll_transmit().unwrap();
        let err = [&[0u8, 0, 4, 1][..], b"Unauthorized"].concat();
        c.handle_input(
            &reply(
                &first,
                Class::Error,
                vec![
                    (attr::ERROR_CODE, err),
                    (attr::REALM, b"sew".to_vec()),
                    (attr::NONCE, b"n1".to_vec()),
                ],
                None,
            ),
            now,
        );
        let second = c.poll_transmit().unwrap();
        let key = long_term_key("user", "sew", "pass");
        assert!(verify_integrity(&second, &key), "authenticated Allocate");
        let d = Message::decode(&second).unwrap();
        assert_eq!(d.text(attr::NONCE).as_deref(), Some("n1"));
        let relayed: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let tid = d.tid;
        c.handle_input(
            &reply(
                &second,
                Class::Success,
                vec![
                    (attr::XOR_RELAYED_ADDRESS, encode_xor_addr(relayed, &tid)),
                    (attr::LIFETIME, 600u32.to_be_bytes().to_vec()),
                ],
                Some(&key),
            ),
            now,
        );
        assert!(
            matches!(c.poll_event(), Some(TurnEvent::Allocated { relayed: r, .. }) if r == relayed)
        );
        let peer: SocketAddr = "127.0.0.1:7882".parse().unwrap();
        c.send_to(peer, b"hello", now);
        let perm = c.poll_transmit().unwrap();
        let bind = c.poll_transmit().unwrap();
        let ind = c.poll_transmit().unwrap();
        assert_eq!(
            Message::decode(&perm).unwrap().method,
            Method::CreatePermission
        );
        let b = Message::decode(&bind).unwrap();
        assert_eq!(b.method, Method::ChannelBind);
        assert_eq!(b.xor_addr(attr::XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(Message::decode(&ind).unwrap().method, Method::Send);
        c.handle_input(&reply(&bind, Class::Success, vec![], Some(&key)), now);
        assert!(matches!(
            c.poll_event(),
            Some(TurnEvent::ChannelBound {
                channel: 0x4000,
                ..
            })
        ));
        c.send_to(peer, b"hello", now);
        let cd = c.poll_transmit().unwrap();
        assert_eq!(decode_channel_data(&cd), Some((0x4000, &b"hello"[..])));
        c.handle_input(&encode_channel_data(0x4000, b"back", false), now);
        assert_eq!(
            c.poll_event(),
            Some(TurnEvent::Data {
                peer,
                data: b"back".to_vec()
            })
        );
        // Refresh at half the lifetime.
        c.handle_timeout(now + Duration::from_secs(301));
        let mut saw_refresh = false;
        while let Some(b) = c.poll_transmit() {
            if Message::decode(&b).map(|m| m.method) == Ok(Method::Refresh) {
                saw_refresh = true;
            }
        }
        assert!(saw_refresh);
        assert!(c.stats().channel_binds >= 1);
    }

    #[test]
    fn udp_requests_retransmit_then_fail() {
        let now = Instant::now();
        let mut c = TurnClient::new("u", "p", false);
        c.start(now);
        let mut t = now;
        let mut sent = 0;
        while let Some(when) = c.poll_timeout() {
            while c.poll_transmit().is_some() {
                sent += 1;
            }
            t = when.max(t);
            c.handle_timeout(t);
            if let Some(ev) = c.poll_event() {
                assert!(matches!(ev, TurnEvent::Failed { code: 0, .. }));
                break;
            }
        }
        assert_eq!(sent, MAX_TRIES);
    }
}
