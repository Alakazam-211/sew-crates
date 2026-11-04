//! The K2 chat pass's optional `caps` claim: an App-wide ceiling over Sew's own permissions.
//!
//! K2 snapshots an App's caps onto the App session at login and puts them in every pass it
//! mints for that session. Sew verifies the claim with the pass, keeps it on the session it
//! creates, and refuses a route that needs a cap the session lacks, whatever its permissions say:
//! the ceiling never grants anything. A pass without the claim is today's behaviour: no ceiling
//! (owner and box passes, Connect sessions).
//!
//! Claim shape, checked by [`parse_caps_claim`]:
//! - `caps` is a JSON array of 1–[`CAPS_MAX`] distinct strings (`null`, an empty array, a
//!   non-array or a non-string entry is refused: the pass is `bad_caps`, `401`);
//! - each entry is 3–[`CAP_MAX_BYTES`] bytes of `<noun>:<verb>`, both `[a-z][a-z0-9_]*`;
//! - `chat:read` must be present (every Sew session reads);
//! - well-formed entries Sew does not know are ignored (they grant nothing).
//!
//! The ceiling of a renewed session is the old ceiling intersected with the new pass's (a
//! renew never widens it; without a claim the old ceiling is kept).

use std::fmt;

use serde_json::Value;

/// The claim's name in the pass.
pub const CAPS_CLAIM: &str = "caps";
/// The optional `app` claim's name: the App the pass's session belongs to, an App id
/// (`sew_proto::mgmt::is_app_id`, 1–64 bytes of `[a-z0-9][a-z0-9._-]*`). Any other value
/// refuses the pass (`bad_app`, `401`). A session with the claim sees only that App's Spaces
/// (or, for the install's home client, every Space but those hidden from home).
pub const APP_CLAIM: &str = "app";
/// Most entries in the claim.
pub const CAPS_MAX: usize = 16;
/// Longest entry, bytes.
pub const CAP_MAX_BYTES: usize = 32;
/// `details.reason` of the `403 missing_permission` a route answers when the session's
/// ceiling lacks the cap it needs (`details.cap` names the cap).
pub const REASON_CAP_CEILING: &str = "cap_ceiling";

/// A cap Sew knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PassCap {
    /// `chat:read`: read what the member's permissions show. Required in every claim.
    ChatRead,
    /// `chat:post`: post, reply, forward and edit messages, and create, edit or send scheduled
    /// messages.
    ChatPost,
    /// `chat:respond`: answer asks. Reserved: accepted and kept, enforced once the answer
    /// routes exist.
    ChatRespond,
    /// `voice:join`: join voice channels and calls, and ring a DM call.
    VoiceJoin,
    /// `voice:video`: camera and screen-share leases. Without it a join is audio-only
    /// (a join's `self_video` is dropped) and a lease request answers the cap refusal.
    /// `voice:join` is still what lets a session join at all.
    VoiceVideo,
}

impl PassCap {
    /// Every cap, in bit order.
    pub const ALL: [PassCap; 5] = [
        PassCap::ChatRead,
        PassCap::ChatPost,
        PassCap::ChatRespond,
        PassCap::VoiceJoin,
        PassCap::VoiceVideo,
    ];

    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            PassCap::ChatRead => "chat:read",
            PassCap::ChatPost => "chat:post",
            PassCap::ChatRespond => "chat:respond",
            PassCap::VoiceJoin => "voice:join",
            PassCap::VoiceVideo => "voice:video",
        }
    }

    /// The cap with this wire string.
    pub fn parse(s: &str) -> Option<PassCap> {
        PassCap::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// What it allows and where Sew enforces it (docs).
    pub const fn enforced(self) -> &'static str {
        match self {
            PassCap::ChatRead => "required in every claim; reading is not gated further",
            PassCap::ChatPost => {
                "POST /api/v1/channels/{id}/messages, /messages/{id}/forward, /messages/{id}/edit, \
                 /channels/{id}/scheduled, /scheduled/{id}/edit, /scheduled/{id}/send_now"
            }
            PassCap::ChatRespond => "reserved for the answer routes (not enforced yet)",
            PassCap::VoiceJoin => {
                "POST /api/v1/channels/{id}/voice/join, /channels/{id}/call/join, \
                 /channels/{id}/call/ring"
            }
            PassCap::VoiceVideo => {
                "camera and screen-share leases: POST /api/v1/me/voice/state with self_video or \
                 self_stream true; without it a voice or call join is audio-only (its \
                 self_video is dropped) and the join answer's media shows camera_allowed and \
                 screen_allowed false"
            }
        }
    }

    const fn bit(self) -> u8 {
        match self {
            PassCap::ChatRead => 1,
            PassCap::ChatPost => 2,
            PassCap::ChatRespond => 4,
            PassCap::VoiceJoin => 8,
            PassCap::VoiceVideo => 16,
        }
    }
}

impl fmt::Display for PassCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of known caps (a session's ceiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CapSet(u8);

impl CapSet {
    /// Every known bit.
    const MASK: u8 = 0b1_1111;

    /// No caps.
    pub const fn empty() -> CapSet {
        CapSet(0)
    }

    /// This set plus `cap`.
    pub const fn with(self, cap: PassCap) -> CapSet {
        CapSet(self.0 | cap.bit())
    }

    /// Whether `cap` is in the set.
    pub const fn contains(self, cap: PassCap) -> bool {
        self.0 & cap.bit() != 0
    }

    /// The caps in both sets.
    pub const fn intersect(self, other: CapSet) -> CapSet {
        CapSet(self.0 & other.0)
    }

    /// The caps, in bit order.
    pub fn iter(self) -> impl Iterator<Item = PassCap> {
        PassCap::ALL.into_iter().filter(move |c| self.contains(*c))
    }

    /// The wire strings, in bit order.
    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(PassCap::as_str).collect()
    }

    /// The stored form (`sessions.caps`).
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// From the stored form; `None` when an unknown bit is set.
    pub const fn from_bits(bits: u8) -> Option<CapSet> {
        if bits & !Self::MASK == 0 {
            Some(CapSet(bits))
        } else {
            None
        }
    }
}

impl FromIterator<PassCap> for CapSet {
    fn from_iter<I: IntoIterator<Item = PassCap>>(iter: I) -> CapSet {
        iter.into_iter().fold(CapSet::empty(), CapSet::with)
    }
}

/// Why a `caps` claim was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsError {
    /// Not an array (`null` included).
    NotArray,
    /// An empty array.
    Empty,
    /// More than [`CAPS_MAX`] entries.
    TooMany,
    /// An entry is not a string.
    NotString,
    /// An entry is not `<noun>:<verb>` of 3–32 bytes.
    Malformed(String),
    /// An entry appears twice.
    Duplicate(String),
    /// `chat:read` is missing.
    NoChatRead,
}

impl fmt::Display for CapsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapsError::NotArray => f.write_str("caps must be an array of strings"),
            CapsError::Empty => f.write_str("caps must not be empty"),
            CapsError::TooMany => write!(f, "caps holds more than {CAPS_MAX} entries"),
            CapsError::NotString => f.write_str("a caps entry is not a string"),
            CapsError::Malformed(s) => write!(f, "caps entry {s:?} is not <noun>:<verb>"),
            CapsError::Duplicate(s) => write!(f, "caps entry {s:?} appears twice"),
            CapsError::NoChatRead => f.write_str("caps must contain chat:read"),
        }
    }
}

impl std::error::Error for CapsError {}

fn word_ok(w: &str) -> bool {
    let b = w.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Whether `s` is a well-formed cap entry: 3–32 bytes of `<noun>:<verb>`.
pub fn is_cap_entry(s: &str) -> bool {
    (3..=CAP_MAX_BYTES).contains(&s.len())
        && s.split_once(':')
            .is_some_and(|(n, v)| word_ok(n) && word_ok(v))
}

/// Parses a present `caps` claim (see the module docs). Unknown well-formed entries are
/// ignored.
pub fn parse_caps_claim(v: &Value) -> Result<CapSet, CapsError> {
    let Value::Array(items) = v else {
        return Err(CapsError::NotArray);
    };
    if items.is_empty() {
        return Err(CapsError::Empty);
    }
    if items.len() > CAPS_MAX {
        return Err(CapsError::TooMany);
    }
    let mut seen: Vec<&str> = Vec::with_capacity(items.len());
    let mut set = CapSet::empty();
    for item in items {
        let Value::String(s) = item else {
            return Err(CapsError::NotString);
        };
        if !is_cap_entry(s) {
            return Err(CapsError::Malformed(s.clone()));
        }
        if seen.contains(&s.as_str()) {
            return Err(CapsError::Duplicate(s.clone()));
        }
        seen.push(s);
        if let Some(c) = PassCap::parse(s) {
            set = set.with(c);
        }
    }
    if !set.contains(PassCap::ChatRead) {
        return Err(CapsError::NoChatRead);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn caps_claim_table() {
        let ok = |v: Value| parse_caps_claim(&v).map(CapSet::names);
        assert_eq!(
            ok(json!([
                "chat:read",
                "chat:post",
                "chat:respond",
                "voice:join"
            ])),
            Ok(vec!["chat:read", "chat:post", "chat:respond", "voice:join"])
        );
        assert_eq!(
            ok(json!(["voice:video", "chat:read", "voice:join"])),
            Ok(vec!["chat:read", "voice:join", "voice:video"])
        );
        assert_eq!(ok(json!(["chat:read"])), Ok(vec!["chat:read"]));
        assert_eq!(
            ok(json!(["voice:join", "chat:read"])),
            Ok(vec!["chat:read", "voice:join"])
        );
        // Unknown but well-formed: ignored.
        assert_eq!(
            ok(json!(["chat:read", "files:upload", "x2:y_3"])),
            Ok(vec!["chat:read"])
        );
        let err = |v: Value| parse_caps_claim(&v).err();
        assert_eq!(err(json!(null)), Some(CapsError::NotArray));
        assert_eq!(err(json!("chat:read")), Some(CapsError::NotArray));
        assert_eq!(err(json!({"chat:read": true})), Some(CapsError::NotArray));
        assert_eq!(err(json!([])), Some(CapsError::Empty));
        assert_eq!(err(json!(["chat:read", 1])), Some(CapsError::NotString));
        assert_eq!(err(json!(["chat:read", null])), Some(CapsError::NotString));
        for bad in [
            "Chat:read",
            "chat:Read",
            "chat",
            "chat:",
            ":read",
            "chat:read:x",
            "chat read",
            "1chat:read",
            "chat:-read",
        ] {
            assert_eq!(
                err(json!(["chat:read", bad])),
                Some(CapsError::Malformed(bad.to_owned())),
                "{bad}"
            );
        }
        let long = format!("chat:{}", "r".repeat(CAP_MAX_BYTES - 4));
        assert_eq!(long.len(), CAP_MAX_BYTES + 1);
        assert_eq!(
            err(json!(["chat:read", long])),
            Some(CapsError::Malformed(long.clone()))
        );
        assert!(is_cap_entry(&long[..CAP_MAX_BYTES]));
        assert_eq!(
            err(json!(["chat:read", "chat:read"])),
            Some(CapsError::Duplicate("chat:read".into()))
        );
        assert_eq!(
            err(json!(["chat:post", "voice:join"])),
            Some(CapsError::NoChatRead)
        );
        let many: Vec<String> = (0..=CAPS_MAX).map(|i| format!("x:y{i}")).collect();
        assert_eq!(err(json!(many)), Some(CapsError::TooMany));
        let mut sixteen: Vec<String> = (1..CAPS_MAX).map(|i| format!("x:y{i}")).collect();
        sixteen.push("chat:read".into());
        assert_eq!(ok(json!(sixteen)), Ok(vec!["chat:read"]));
    }

    #[test]
    fn cap_set_bits() {
        let all: CapSet = PassCap::ALL.into_iter().collect();
        assert_eq!(all.bits(), 0b1_1111);
        for c in PassCap::ALL {
            assert_eq!(PassCap::parse(c.as_str()), Some(c));
            let one = CapSet::empty().with(c);
            assert_eq!(CapSet::from_bits(one.bits()), Some(one));
            assert!(one.contains(c));
        }
        assert_eq!(CapSet::from_bits(0b10_0000), None);
        assert_eq!(PassCap::parse("voice:video"), Some(PassCap::VoiceVideo));
        let a = CapSet::empty()
            .with(PassCap::ChatRead)
            .with(PassCap::VoiceJoin);
        let b = CapSet::empty()
            .with(PassCap::ChatRead)
            .with(PassCap::ChatPost);
        assert_eq!(a.intersect(b).names(), vec!["chat:read"]);
    }
}
