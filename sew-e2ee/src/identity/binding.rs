//! The DS-signed `LeafBinding` table.
//!
//! The SDKs key everything by the engine identity (`p_…`), so the client must know which leaf is
//! which identity. [`verify_table`] is the stateless part (what the recorder runs): the DS
//! signature, every `lk_identity` = `"p_" + base32(join_id)`, one key type, ascending keys, unique
//! join ids (so unique identities). The checks that need the tree or the session (coverage, a live
//! leaf's identity never changes, the local entry) live in `CallSession`.
//!
//! Keys are derived per **leaf index**, so a swapped table yields decrypt failures, never
//! one person's media under another's name.

use std::collections::BTreeMap;

use sew_proto::e2ee::tls::BindingKey;
use sew_proto::e2ee::{LeafBindingTable, wire};

use super::cert::verify_bindings_sig;
use crate::config::caps;
use crate::error::{BindingError, DecodeError};
use crate::ids::{JoinId, LkIdentity, RoomName};

/// One verified entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEntry {
    /// The join id.
    pub join_id: JoinId,
    /// Its engine identity (recomputed and compared).
    pub lk_identity: LkIdentity,
}

/// A table whose signature and content rules passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTable {
    /// The call.
    pub call_id: RoomName,
    /// The group.
    pub group_id: Vec<u8>,
    /// The epoch it is for.
    pub epoch: u64,
    /// Entries keyed by leaf (key type 1).
    pub by_leaf: BTreeMap<u32, VerifiedEntry>,
    /// Entries keyed by `key_package_ref` (key type 2).
    pub by_ref: BTreeMap<Vec<u8>, VerifiedEntry>,
}

impl VerifiedTable {
    /// Whether this is a key-type-1 table (or empty).
    pub fn is_leaf_table(&self) -> bool {
        self.by_ref.is_empty()
    }

    /// Whether this is a key-type-2 table (or empty).
    pub fn is_ref_table(&self) -> bool {
        self.by_leaf.is_empty()
    }

    /// Entries.
    pub fn len(&self) -> usize {
        self.by_leaf.len() + self.by_ref.len()
    }

    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Verifies a signed table statelessly (the DS signature and the identities, plus uniqueness of
/// leaves, refs, join ids and identities).
pub fn verify_table(
    t: &LeafBindingTable,
    ds_public_key: &[u8; 32],
) -> Result<VerifiedTable, BindingError> {
    if t.entries.len() > caps::LEAF_BINDING_ENTRIES {
        return Err(BindingError::Decode(DecodeError::TooLarge {
            what: "leaf_binding_table",
            len: t.entries.len(),
            cap: caps::LEAF_BINDING_ENTRIES,
        }));
    }
    let tbs = t.tbs().map_err(|e| match e {
        wire::BindingError::Identity(_) => BindingError::Identity,
        wire::BindingError::CallId => BindingError::Scope("call_id"),
        wire::BindingError::EntryKey(_) | wire::BindingError::Table(_) => BindingError::Duplicate,
    })?;
    let bytes_len = tbs.to_bytes_len();
    if bytes_len > caps::LEAF_BINDING_TABLE {
        return Err(BindingError::Decode(DecodeError::TooLarge {
            what: "leaf_binding_table",
            len: bytes_len,
            cap: caps::LEAF_BINDING_TABLE,
        }));
    }
    if !verify_bindings_sig(ds_public_key, &tbs, t.sig.as_bytes()) {
        return Err(BindingError::Signature);
    }
    let mut by_leaf = BTreeMap::new();
    let mut by_ref = BTreeMap::new();
    for (e, json) in tbs.entries.iter().zip(&t.entries) {
        if json.lk_identity != json.join_id.lk_identity() {
            return Err(BindingError::Identity);
        }
        let v = VerifiedEntry {
            join_id: json.join_id.clone(),
            lk_identity: json.lk_identity.clone(),
        };
        let dup = match &e.key {
            BindingKey::Leaf(l) => by_leaf.insert(*l, v).is_some(),
            BindingKey::KeyPackageRef(r) => by_ref.insert(r.clone(), v).is_some(),
        };
        if dup {
            return Err(BindingError::Duplicate);
        }
    }
    Ok(VerifiedTable {
        call_id: t.call_id.clone(),
        group_id: t.group_id.as_bytes().to_vec(),
        epoch: t.epoch,
        by_leaf,
        by_ref,
    })
}

trait TbsLen {
    fn to_bytes_len(&self) -> usize;
}

impl TbsLen for sew_proto::e2ee::tls::LeafBindingTableTbs {
    fn to_bytes_len(&self) -> usize {
        use sew_proto::e2ee::tls::TlsCodec as _;
        self.to_bytes().map(|b| b.len()).unwrap_or(usize::MAX)
    }
}
