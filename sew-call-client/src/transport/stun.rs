//! (Copied from Sew's media test client, not published: Alakazam Labs' own code.)
//!
//! A minimal STUN/TURN message codec (RFC 8489, RFC 8656) for the test client's TURN client.
//! Written from the RFCs; only what a TURN client needs: long-term credentials with
//! MESSAGE-INTEGRITY (HMAC-SHA1, key = MD5(username ":" realm ":" password)), the XOR address
//! attributes, and ChannelData framing.

// Not every codec item is used by the engine (the test client uses more of it).
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use aws_lc_rs::hmac;

/// The STUN magic cookie.
pub(crate) const MAGIC_COOKIE: u32 = 0x2112_A442;

/// STUN methods used here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Method {
    /// Binding (0x001).
    Binding,
    /// Allocate (0x003).
    Allocate,
    /// Refresh (0x004).
    Refresh,
    /// Send (0x006, indication).
    Send,
    /// Data (0x007, indication).
    Data,
    /// CreatePermission (0x008).
    CreatePermission,
    /// ChannelBind (0x009).
    ChannelBind,
    /// Anything else.
    Other(u16),
}

impl Method {
    fn code(self) -> u16 {
        match self {
            Method::Binding => 0x001,
            Method::Allocate => 0x003,
            Method::Refresh => 0x004,
            Method::Send => 0x006,
            Method::Data => 0x007,
            Method::CreatePermission => 0x008,
            Method::ChannelBind => 0x009,
            Method::Other(c) => c,
        }
    }

    fn from_code(c: u16) -> Method {
        match c {
            0x001 => Method::Binding,
            0x003 => Method::Allocate,
            0x004 => Method::Refresh,
            0x006 => Method::Send,
            0x007 => Method::Data,
            0x008 => Method::CreatePermission,
            0x009 => Method::ChannelBind,
            c => Method::Other(c),
        }
    }
}

/// STUN message classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    /// Request.
    Request,
    /// Indication.
    Indication,
    /// Success response.
    Success,
    /// Error response.
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Class::Request => 0b00,
            Class::Indication => 0b01,
            Class::Success => 0b10,
            Class::Error => 0b11,
        }
    }
}

/// Encodes a message type from method and class (RFC 8489 §5).
pub(crate) fn message_type(method: Method, class: Class) -> u16 {
    let m = method.code();
    let c = class.bits();
    (m & 0x000F) | ((m & 0x0070) << 1) | ((m & 0x0F80) << 2) | ((c & 1) << 4) | ((c & 2) << 7)
}

/// Decodes a message type into method and class.
pub(crate) fn split_type(t: u16) -> (Method, Class) {
    let m = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let c = ((t >> 4) & 1) | ((t >> 7) & 2);
    let class = match c {
        0 => Class::Request,
        1 => Class::Indication,
        2 => Class::Success,
        _ => Class::Error,
    };
    (Method::from_code(m), class)
}

/// Attribute types.
pub(crate) mod attr {
    /// MAPPED-ADDRESS.
    pub(crate) const MAPPED_ADDRESS: u16 = 0x0001;
    /// USERNAME.
    pub(crate) const USERNAME: u16 = 0x0006;
    /// MESSAGE-INTEGRITY.
    pub(crate) const MESSAGE_INTEGRITY: u16 = 0x0008;
    /// ERROR-CODE.
    pub(crate) const ERROR_CODE: u16 = 0x0009;
    /// CHANNEL-NUMBER.
    pub(crate) const CHANNEL_NUMBER: u16 = 0x000C;
    /// LIFETIME.
    pub(crate) const LIFETIME: u16 = 0x000D;
    /// XOR-PEER-ADDRESS.
    pub(crate) const XOR_PEER_ADDRESS: u16 = 0x0012;
    /// DATA.
    pub(crate) const DATA: u16 = 0x0013;
    /// REALM.
    pub(crate) const REALM: u16 = 0x0014;
    /// NONCE.
    pub(crate) const NONCE: u16 = 0x0015;
    /// XOR-RELAYED-ADDRESS.
    pub(crate) const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    /// REQUESTED-TRANSPORT.
    pub(crate) const REQUESTED_TRANSPORT: u16 = 0x0019;
    /// XOR-MAPPED-ADDRESS.
    pub(crate) const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    /// SOFTWARE.
    pub(crate) const SOFTWARE: u16 = 0x8022;
    /// FINGERPRINT.
    pub(crate) const FINGERPRINT: u16 = 0x8028;
}

/// A decoded or to-be-encoded STUN message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    /// Method.
    pub(crate) method: Method,
    /// Class.
    pub(crate) class: Class,
    /// Transaction id.
    pub(crate) tid: [u8; 12],
    /// Attributes in order (type, value), excluding MESSAGE-INTEGRITY and FINGERPRINT.
    pub(crate) attrs: Vec<(u16, Vec<u8>)>,
    /// Whether the decoded message carried MESSAGE-INTEGRITY.
    pub(crate) has_integrity: bool,
}

/// Why bytes are not a STUN message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StunError {
    /// Too short or bad length.
    #[error("malformed STUN message")]
    Malformed,
    /// Not STUN (no magic cookie / top bits).
    #[error("not a STUN message")]
    NotStun,
}

impl Message {
    /// A new message with a random transaction id.
    pub(crate) fn new(method: Method, class: Class) -> Message {
        Message {
            method,
            class,
            tid: rand::random(),
            attrs: Vec::new(),
            has_integrity: false,
        }
    }

    /// Adds an attribute.
    pub(crate) fn with(mut self, t: u16, v: impl Into<Vec<u8>>) -> Message {
        self.attrs.push((t, v.into()));
        self
    }

    /// The first attribute of type `t`.
    pub(crate) fn get(&self, t: u16) -> Option<&[u8]> {
        self.attrs
            .iter()
            .find(|(k, _)| *k == t)
            .map(|(_, v)| v.as_slice())
    }

    /// A text attribute.
    pub(crate) fn text(&self, t: u16) -> Option<String> {
        self.get(t).map(|v| String::from_utf8_lossy(v).into_owned())
    }

    /// An XOR address attribute.
    pub(crate) fn xor_addr(&self, t: u16) -> Option<SocketAddr> {
        self.get(t).and_then(|v| decode_xor_addr(v, &self.tid))
    }

    /// The ERROR-CODE (code, reason).
    pub(crate) fn error_code(&self) -> Option<(u16, String)> {
        let v = self.get(attr::ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        let code = u16::from(v[2] & 0x7) * 100 + u16::from(v[3]);
        Some((code, String::from_utf8_lossy(&v[4..]).into_owned()))
    }

    /// Encodes with an optional long-term key for MESSAGE-INTEGRITY.
    pub(crate) fn encode(&self, key: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&message_type(self.method, self.class).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.tid);
        for (t, v) in &self.attrs {
            push_attr(&mut out, *t, v);
        }
        if let Some(key) = key {
            // Length includes the MESSAGE-INTEGRITY attribute (RFC 8489 §14.5).
            let len = (out.len() - 20 + 24) as u16;
            out[2..4].copy_from_slice(&len.to_be_bytes());
            let k = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
            let tag = hmac::sign(&k, &out);
            push_attr(&mut out, attr::MESSAGE_INTEGRITY, tag.as_ref());
        }
        let len = (out.len() - 20) as u16;
        out[2..4].copy_from_slice(&len.to_be_bytes());
        out
    }

    /// Decodes one message (the whole buffer must be the message).
    pub(crate) fn decode(b: &[u8]) -> Result<Message, StunError> {
        if b.len() < 20 || b[0] & 0xC0 != 0 {
            return Err(StunError::NotStun);
        }
        if u32::from_be_bytes([b[4], b[5], b[6], b[7]]) != MAGIC_COOKIE {
            return Err(StunError::NotStun);
        }
        let len = usize::from(u16::from_be_bytes([b[2], b[3]]));
        if b.len() < 20 + len || !len.is_multiple_of(4) {
            return Err(StunError::Malformed);
        }
        let (method, class) = split_type(u16::from_be_bytes([b[0], b[1]]));
        let mut tid = [0u8; 12];
        tid.copy_from_slice(&b[8..20]);
        let mut attrs = Vec::new();
        let mut has_integrity = false;
        let mut at = 20;
        while at + 4 <= 20 + len {
            let t = u16::from_be_bytes([b[at], b[at + 1]]);
            let l = usize::from(u16::from_be_bytes([b[at + 2], b[at + 3]]));
            let v = b.get(at + 4..at + 4 + l).ok_or(StunError::Malformed)?;
            match t {
                attr::MESSAGE_INTEGRITY => has_integrity = true,
                attr::FINGERPRINT => {}
                _ => attrs.push((t, v.to_vec())),
            }
            at += 4 + l.div_ceil(4) * 4;
        }
        Ok(Message {
            method,
            class,
            tid,
            attrs,
            has_integrity,
        })
    }
}

fn push_attr(out: &mut Vec<u8>, t: u16, v: &[u8]) {
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&(v.len() as u16).to_be_bytes());
    out.extend_from_slice(v);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Encodes an XOR address attribute value.
pub(crate) fn encode_xor_addr(addr: SocketAddr, tid: &[u8; 12]) -> Vec<u8> {
    let port = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
    let mut v = vec![0u8];
    match addr.ip() {
        IpAddr::V4(ip) => {
            v.push(1);
            v.extend_from_slice(&port.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC_COOKIE;
            v.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            v.push(2);
            v.extend_from_slice(&port.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(tid);
            v.extend(ip.octets().iter().zip(mask).map(|(a, m)| a ^ m));
        }
    }
    v
}

/// Decodes an XOR address attribute value.
pub(crate) fn decode_xor_addr(v: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ ((MAGIC_COOKIE >> 16) as u16);
    match v[1] {
        1 => {
            let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC_COOKIE;
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), port))
        }
        2 if v.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(tid);
            let mut o = [0u8; 16];
            for (dst, (a, m)) in o.iter_mut().zip(v[4..20].iter().zip(mask)) {
                *dst = a ^ m;
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port))
        }
        _ => None,
    }
}

/// The long-term credential key: MD5(username ":" realm ":" password) (RFC 8489 §9.2.2).
pub(crate) fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    md5(format!("{username}:{realm}:{password}").as_bytes())
}

/// Whether `b` starts a ChannelData message (first two bits 01, RFC 8656 §12.4).
pub(crate) fn is_channel_data(b: &[u8]) -> bool {
    b.len() >= 4 && (0x40..=0x4F).contains(&b[0])
}

/// Encodes ChannelData; `pad` pads to a multiple of 4 (required over TCP/TLS).
pub(crate) fn encode_channel_data(channel: u16, data: &[u8], pad: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    if pad {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    out
}

/// Decodes ChannelData into (channel, payload).
pub(crate) fn decode_channel_data(b: &[u8]) -> Option<(u16, &[u8])> {
    if !is_channel_data(b) {
        return None;
    }
    let ch = u16::from_be_bytes([b[0], b[1]]);
    let len = usize::from(u16::from_be_bytes([b[2], b[3]]));
    b.get(4..4 + len).map(|d| (ch, d))
}

/// Splits a TCP/TLS byte stream to a TURN server into whole STUN and ChannelData messages
/// (RFC 8656 §12.5: ChannelData is padded to 4 bytes over stream transports).
#[derive(Debug, Default)]
pub(crate) struct StreamFramer {
    buf: Vec<u8>,
}

impl StreamFramer {
    /// Appends received bytes.
    pub(crate) fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole message, if buffered.
    pub(crate) fn next_message(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < 4 {
            return None;
        }
        let total = if is_channel_data(&self.buf) {
            let len = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
            (4 + len).div_ceil(4) * 4
        } else {
            let len = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
            20 + len
        };
        if self.buf.len() < total {
            return None;
        }
        let rest = self.buf.split_off(total);
        Some(std::mem::replace(&mut self.buf, rest))
    }
}

/// MD5 (RFC 1321), only for the TURN long-term credential key.
pub(crate) fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i: i32| ((f64::from(i + 1)).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, w) in chunk.chunks(4).enumerate() {
            m[i] = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Verifies MESSAGE-INTEGRITY of raw bytes with `key` (for tests and server replies).
pub(crate) fn verify_integrity(b: &[u8], key: &[u8]) -> bool {
    let Ok(_) = Message::decode(b) else {
        return false;
    };
    let mut at = 20;
    while at + 4 <= b.len() {
        let t = u16::from_be_bytes([b[at], b[at + 1]]);
        let l = usize::from(u16::from_be_bytes([b[at + 2], b[at + 3]]));
        if t == attr::MESSAGE_INTEGRITY {
            if l != 20 || b.len() < at + 24 {
                return false;
            }
            let mut head = b[..at].to_vec();
            let len = (at - 20 + 24) as u16;
            head[2..4].copy_from_slice(&len.to_be_bytes());
            let k = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
            return hmac::verify(&k, &head, &b[at + 4..at + 24]).is_ok();
        }
        at += 4 + l.div_ceil(4) * 4;
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn md5_known_vectors() {
        assert_eq!(hex::encode(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            hex::encode(md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        // RFC 5769 §2.4: key = MD5("<U+30DE><U+30C8><U+30EA><U+30C3><U+30AF><U+30B9>:example.org:TheMatrIX")
        assert_eq!(
            hex::encode(long_term_key("user", "realm", "pass")),
            hex::encode(md5(b"user:realm:pass"))
        );
    }

    #[test]
    fn message_type_roundtrip() {
        assert_eq!(message_type(Method::Binding, Class::Request), 0x0001);
        assert_eq!(message_type(Method::Binding, Class::Success), 0x0101);
        assert_eq!(message_type(Method::Allocate, Class::Request), 0x0003);
        assert_eq!(message_type(Method::Allocate, Class::Error), 0x0113);
        assert_eq!(message_type(Method::Data, Class::Indication), 0x0017);
        for m in [
            Method::Allocate,
            Method::ChannelBind,
            Method::CreatePermission,
        ] {
            for c in [
                Class::Request,
                Class::Indication,
                Class::Success,
                Class::Error,
            ] {
                assert_eq!(split_type(message_type(m, c)), (m, c));
            }
        }
    }

    #[test]
    fn turn_message_encode_decode_with_integrity() {
        let peer: SocketAddr = "10.1.2.3:4567".parse().unwrap();
        let peer6: SocketAddr = "[2001:db8::1]:9".parse().unwrap();
        let m = Message::new(Method::ChannelBind, Class::Request);
        let tid = m.tid;
        let m = m
            .with(attr::CHANNEL_NUMBER, vec![0x40, 0x00, 0, 0])
            .with(attr::XOR_PEER_ADDRESS, encode_xor_addr(peer, &tid))
            .with(attr::XOR_RELAYED_ADDRESS, encode_xor_addr(peer6, &tid))
            .with(attr::USERNAME, "u")
            .with(attr::REALM, "r");
        let key = long_term_key("u", "r", "p");
        let bytes = m.encode(Some(&key));
        assert!(verify_integrity(&bytes, &key));
        assert!(!verify_integrity(&bytes, &long_term_key("u", "r", "x")));
        let d = Message::decode(&bytes).unwrap();
        assert!(d.has_integrity);
        assert_eq!(d.method, Method::ChannelBind);
        assert_eq!(d.xor_addr(attr::XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(d.xor_addr(attr::XOR_RELAYED_ADDRESS), Some(peer6));
        assert_eq!(d.text(attr::USERNAME).as_deref(), Some("u"));
    }

    #[test]
    fn error_code_and_channel_data_framing() {
        let m = Message::new(Method::Allocate, Class::Error).with(
            attr::ERROR_CODE,
            [&[0u8, 0, 4, 1][..], b"Unauthorized"].concat(),
        );
        let d = Message::decode(&m.encode(None)).unwrap();
        assert_eq!(d.error_code(), Some((401, "Unauthorized".to_owned())));
        let cd = encode_channel_data(0x4001, b"abcde", true);
        assert_eq!(cd.len(), 12);
        assert_eq!(decode_channel_data(&cd), Some((0x4001, &b"abcde"[..])));
        let mut f = StreamFramer::default();
        let stun = Message::new(Method::Refresh, Class::Success).encode(None);
        let mut all = cd.clone();
        all.extend_from_slice(&stun);
        f.push(&all[..7]);
        assert_eq!(f.next_message(), None);
        f.push(&all[7..]);
        assert_eq!(f.next_message(), Some(cd));
        assert_eq!(f.next_message(), Some(stun));
        assert_eq!(f.next_message(), None);
    }
}
