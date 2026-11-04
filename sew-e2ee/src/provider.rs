//! `CallProvider`: sew-mls-crypto's `MlsCrypto` (openmls' crypto traits, HPKE without
//! hpke-rs) with an in-memory store that is zeroized on drop.
//!
//! Limit: openmls replaces stored values on each write and the replaced buffers are freed
//! without zeroizing; a zeroizing `StorageProvider` is a later change.

use std::fmt;

use openmls_memory_storage::MemoryStorage;
use openmls_traits::OpenMlsProvider;
use sew_mls_crypto::MlsCrypto;
use zeroize::Zeroize;

/// The provider one call's MLS state lives in. Nothing about a call is ever persisted.
#[derive(Default)]
pub struct CallProvider {
    crypto: MlsCrypto,
    storage: MemoryStorage,
}

/// `openmls_memory_storage` 0.6.0's label of a queued proposal's entry.
const QUEUED_PROPOSAL_LABEL: &[u8] = b"QueuedProposal";

impl CallProvider {
    /// An empty provider.
    pub fn new() -> CallProvider {
        CallProvider::default()
    }

    /// Stored entries (for tests and telemetry; no contents).
    pub fn stored_entries(&self) -> usize {
        self.storage
            .values
            .read()
            .map(|v| v.len())
            .unwrap_or_else(|p| p.into_inner().len())
    }

    /// Bytes of the stored keys and values (telemetry).
    pub fn stored_bytes(&self) -> usize {
        let sum = |v: &std::collections::HashMap<Vec<u8>, Vec<u8>>| {
            v.iter().map(|(k, x)| k.len() + x.len()).sum::<usize>()
        };
        self.storage
            .values
            .read()
            .map(|v| sum(&v))
            .unwrap_or_else(|p| sum(&p.into_inner()))
    }

    /// Stored entries by openmls storage label (the key's leading ASCII letters; telemetry, no
    /// contents).
    pub fn stored_labels(&self) -> std::collections::BTreeMap<String, usize> {
        let count = |v: &std::collections::HashMap<Vec<u8>, Vec<u8>>| {
            let mut out = std::collections::BTreeMap::new();
            for k in v.keys() {
                let label: String = k
                    .iter()
                    .take_while(|b| b.is_ascii_alphabetic() || **b == b'_')
                    .map(|b| char::from(*b))
                    .collect();
                *out.entry(label).or_insert(0) += 1;
            }
            out
        };
        self.storage
            .values
            .read()
            .map(|v| count(&v))
            .unwrap_or_else(|p| count(&p.into_inner()))
    }

    /// [`purge_cleared_proposals`] on this provider's store.
    pub(crate) fn purge_cleared_proposals(&self, group: &openmls::prelude::GroupId) {
        purge_cleared_proposals(&self.storage, group);
    }

    /// Zeroizes and drops every stored key and value now.
    pub fn wipe(&self) {
        let mut guard = match self.storage.values.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for (mut k, mut v) in guard.drain() {
            k.zeroize();
            v.zeroize();
        }
    }
}

/// Deletes the queued proposals of `group` that openmls cleared from its queue but left in
/// the store (zeroized), after a merged commit or an abandoned transition.
///
/// `openmls_memory_storage` 0.6.0's `clear_proposal_queue` deletes each proposal
/// under its bare key (`serde_json((group, ref))`) instead of the labelled key `queue_proposal`
/// wrote it under (`QueuedProposal` + that + the version), so every committed or abandoned
/// proposal stayed in the store for the life of the call: about one entry per epoch in a call
/// with joins and leaves (test `mls_store_flat_under_join_leave_churn`).
pub fn purge_cleared_proposals(storage: &MemoryStorage, group: &openmls::prelude::GroupId) {
    let Ok(g) = serde_json::to_vec(group) else {
        return;
    };
    let mut prefix = QUEUED_PROPOSAL_LABEL.to_vec();
    prefix.push(b'[');
    prefix.extend_from_slice(&g);
    prefix.push(b',');
    let mut guard = match storage.values.write() {
        Ok(v) => v,
        Err(p) => p.into_inner(),
    };
    let stale: Vec<Vec<u8>> = guard
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    for k in stale {
        if let Some((mut key, mut value)) = guard.remove_entry(&k) {
            key.zeroize();
            value.zeroize();
        }
    }
}

impl Drop for CallProvider {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for CallProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallProvider")
            .field("entries", &self.stored_entries())
            .finish()
    }
}

impl OpenMlsProvider for CallProvider {
    type CryptoProvider = MlsCrypto;
    type RandProvider = MlsCrypto;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}
