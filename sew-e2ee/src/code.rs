//! The 30-digit security code: derived from the **active** epoch's
//! `epoch_authenticator()` (RFC 9420 §8.7). Six groups of five digits: group `i` is the
//! big-endian integer of authenticator bytes `[5i, 5i+5)` mod 100,000, zero-padded. Identical on
//! every member of one active epoch; changes every epoch; never modal; not secret, but
//! never logged.

use std::fmt;

/// A security code (6 × 5 digits).
#[derive(Clone, PartialEq, Eq)]
pub struct SecurityCode([u32; 6]);

impl SecurityCode {
    /// The code of an epoch authenticator (at least 30 bytes; SHA-256 suites give 32).
    pub fn from_authenticator(auth: &[u8]) -> Option<SecurityCode> {
        if auth.len() < 30 {
            return None;
        }
        let mut groups = [0u32; 6];
        for (i, g) in groups.iter_mut().enumerate() {
            let mut v: u64 = 0;
            for b in &auth[5 * i..5 * i + 5] {
                v = (v << 8) | u64::from(*b);
            }
            *g = (v % 100_000) as u32;
        }
        Some(SecurityCode(groups))
    }

    /// The six groups.
    pub fn groups(&self) -> [u32; 6] {
        self.0
    }

    /// `"01234 56789 …"`.
    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(|g| format!("{g:05}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The 30 digits without spaces.
    pub fn digits(&self) -> String {
        self.0.iter().map(|g| format!("{g:05}")).collect()
    }
}

impl fmt::Debug for SecurityCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not secret, but kept out of logs.
        f.write_str("SecurityCode(…)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_five_digits() {
        let auth = [0xffu8; 32];
        let c = SecurityCode::from_authenticator(&auth).unwrap_or(SecurityCode([0; 6]));
        // 0xffffffffff = 1099511627775 → mod 100000 = 27775
        assert_eq!(c.groups(), [27_775; 6]);
        assert_eq!(c.digits().len(), 30);
        assert!(SecurityCode::from_authenticator(&[0; 29]).is_none());
    }
}
