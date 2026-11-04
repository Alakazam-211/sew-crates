//! Typed errors. No error ever holds key material: variants carry ids, epochs,
//! slots, sizes and short static reasons only.

use thiserror::Error;

/// A decoder input over its cap ([`crate::config`]), or one that did not decode.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    /// Over the cap; nothing was parsed.
    #[error("{what}: {len} bytes over the cap of {cap}")]
    TooLarge {
        /// The object.
        what: &'static str,
        /// Its size.
        len: usize,
        /// The cap.
        cap: usize,
    },
    /// Malformed.
    #[error("{0}: malformed")]
    Malformed(&'static str),
}

/// Checks `len ≤ cap` before any parse.
pub fn check_cap(what: &'static str, len: usize, cap: usize) -> Result<(), DecodeError> {
    if len > cap {
        Err(DecodeError::TooLarge { what, len, cap })
    } else {
        Ok(())
    }
}

/// Trust-anchor errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TrustError {
    /// A pinned CA, DS key or `external_sender` changed: `E2eeStatus::ServerIdentityChanged`.
    #[error("the server's call identity changed ({what})")]
    ServerIdentityChanged {
        /// `ca` or `external_sender` (the `report` kind follows from it).
        what: &'static str,
    },
    /// READY's `call_keys` is malformed.
    #[error("READY call_keys: {0}")]
    Malformed(&'static str),
    /// The device store failed.
    #[error("device store: {0}")]
    Store(String),
}

/// Device identity and enrollment errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    /// The certificate is for another member.
    #[error("the certificate names another member")]
    WrongMember,
    /// The certificate's key is not this device's key.
    #[error("the certificate names another device key")]
    WrongKey,
    /// The certificate failed a check (decode, CA signature, install, validity, CRL).
    #[error("device certificate refused: {0}")]
    BadCert(&'static str),
    /// Not enrolled, or the stored certificate is no longer usable.
    #[error("the device is not enrolled")]
    NotEnrolled,
    /// A device signature did not verify.
    #[error("the device signature does not verify")]
    BadSignature,
    /// The returned `key_package_ref` is not this package's `hash_ref`.
    #[error("the key_package_ref does not match the package")]
    RefMismatch,
    /// The package has not been uploaded (no ref yet).
    #[error("the key package has no ref (upload it first)")]
    NoRef,
    /// The package is stale (over 60 min old or past the server TTL).
    #[error("the key package is stale")]
    Stale,
    /// MLS refused to build the package.
    #[error("MLS: {0}")]
    Mls(&'static str),
    /// The server's identity changed (no joins).
    #[error("the server's call identity changed")]
    ServerIdentityChanged,
    /// The store failed.
    #[error("device store: {0}")]
    Store(String),
}

/// Why a session could not start or continue.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// A `params` value outside its allowed range.
    #[error("params: {key} out of range")]
    Params {
        /// The key.
        key: &'static str,
    },
    /// `key_ring_size` is not 64.
    #[error("key_ring_size must be 64, got {got}")]
    RingSize {
        /// The value received.
        got: u32,
    },
    /// `CallKeysInit.lk_identity` is not `p_` + base32(`join_id`).
    #[error("lk_identity does not match join_id")]
    IdentityMismatch,
    /// The `external_sender` differs from the pinned bytes.
    #[error("external_sender differs from the pin")]
    ExternalSenderMismatch,
    /// A ciphersuite other than 0x0001.
    #[error("unsupported ciphersuite {0:#06x}")]
    UnsupportedCiphersuite(u16),
    /// The session has left (terminal).
    #[error("the session has left")]
    Left,
    /// An input that does not apply in this state (ignored; the state is unchanged).
    #[error("ignored: {0}")]
    Ignored(&'static str),
    /// A decoder refused its input.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// An MLS operation failed; recovery output is already queued.
    #[error("MLS: {0}")]
    Mls(&'static str),
    /// A message failed a member-side check; recovery output is already queued.
    #[error("refused: {0}")]
    Refused(&'static str),
    /// The device identity is unusable.
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// Why a join failed for good.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JoinError {
    /// `422 e2ee_unsupported_client` with `details.unsupported`.
    #[error("this client cannot join encrypted calls ({unsupported})")]
    Unsupported {
        /// `sender_key_index`, `kdf`, `key_ring_size`, `browser`, …
        unsupported: String,
    },
    /// The key-package ref was refused twice (`e2ee_key_package_ref_unknown` / `_used`).
    #[error("the key package was refused twice")]
    KeyPackage,
    /// The session could not start.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Why a recorder release could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReleaseError {
    /// `enc` is not 32 bytes or `ct` not 48.
    #[error("release sizes")]
    Sizes,
    /// The `info` binding or the ciphertext does not match (AEAD failure).
    #[error("release does not open")]
    Aead,
    /// The release names a malformed slot or identity.
    #[error("release: {0}")]
    Malformed(&'static str),
}

/// Why a signed `LeafBinding` table was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BindingError {
    /// The DS signature does not verify for this call, group and epoch.
    #[error("binding table signature")]
    Signature,
    /// An identity does not match `^p_[a-z2-7]{26}$` or `p_ + base32(join_id)`.
    #[error("binding table identity")]
    Identity,
    /// A leaf, ref, join id or identity appears twice, or keys are mixed or unordered.
    #[error("binding table duplicate or order")]
    Duplicate,
    /// The table is for another call, group or epoch, or has the wrong key type.
    #[error("binding table scope: {0}")]
    Scope(&'static str),
    /// A tree leaf has no entry, or an entry names no leaf of the tree.
    #[error("binding table does not cover the tree")]
    Coverage,
    /// A live leaf's identity changed.
    #[error("binding table changes a live leaf's identity")]
    Changed,
    /// The local leaf's entry is not this session's join id.
    #[error("binding table names another join for the local leaf")]
    Local,
    /// Over the size cap.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

/// Why a recording chunk could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChunkError {
    /// The 16-byte header is malformed (magic, version, chunk size, reserved).
    #[error("recording header: {0}")]
    Header(&'static str),
    /// The ciphertext is shorter than a tag or longer than a chunk.
    #[error("recording chunk length")]
    Length,
    /// The tag does not verify (wrong key, index, `last` flag or tampering).
    #[error("recording chunk does not open")]
    Aead,
    /// The file length does not fit the stream format.
    #[error("recording file length")]
    FileLength,
}
