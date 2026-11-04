//! What persists: the device's certificate and ids, and the TOFU pins of the
//! install's CA key, DS key and `external_sender` bytes. Nothing about a call is ever written
//! (an app restart mid-call goes through the missed-epoch path).
//!
//! The private device key is the platform's (`DeviceSigner`); stores hold public data only.

use std::collections::BTreeMap;

use crate::ids::Id;

/// The device's enrolled state (public data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDevice {
    /// The install.
    pub install_id: u64,
    /// The member the device belongs to.
    pub member_id: Id,
    /// The device (Sew id).
    pub device_id: Id,
    /// The device's 65-byte SEC1 public key.
    pub pubkey: [u8; 65],
    /// TLS `SewDeviceCert`.
    pub cert: Vec<u8>,
}

/// The TOFU pins of one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    /// The install.
    pub install_id: u64,
    /// `sew-call-ca`'s Ed25519 key.
    pub ca_public: [u8; 32],
    /// `sew-call-ds`'s Ed25519 key.
    pub ds_public: [u8; 32],
    /// The TLS `ExternalSender` bytes.
    pub external_sender: Vec<u8>,
}

/// Where a device's state lives (web: IndexedDB `sew-e2ee` v1, store `device`, key = install
/// id). Native stores are synchronous; the TS wrapper adapts the async IndexedDB one.
pub trait DeviceStore {
    /// The device of `install_id`, if enrolled.
    fn load(&self, install_id: u64) -> Option<StoredDevice>;
    /// Saves (replaces) the device.
    fn save(&mut self, dev: &StoredDevice) -> Result<(), String>;
    /// Forgets the device **and** the pins of `install_id` (a new enrollment accepts new pins).
    fn clear(&mut self, install_id: u64);
    /// The pins of `install_id`, if any.
    fn load_pins(&self, install_id: u64) -> Option<Pins>;
    /// Saves the pins (first sight only; a change is refused before this is called).
    fn save_pins(&mut self, pins: &Pins) -> Result<(), String>;
}

/// An in-memory store (tests, the driver).
#[derive(Debug, Default, Clone)]
pub struct MemoryDeviceStore {
    devices: BTreeMap<u64, StoredDevice>,
    pins: BTreeMap<u64, Pins>,
}

impl MemoryDeviceStore {
    /// An empty store.
    pub fn new() -> MemoryDeviceStore {
        MemoryDeviceStore::default()
    }
}

impl DeviceStore for MemoryDeviceStore {
    fn load(&self, install_id: u64) -> Option<StoredDevice> {
        self.devices.get(&install_id).cloned()
    }

    fn save(&mut self, dev: &StoredDevice) -> Result<(), String> {
        self.devices.insert(dev.install_id, dev.clone());
        Ok(())
    }

    fn clear(&mut self, install_id: u64) {
        self.devices.remove(&install_id);
        self.pins.remove(&install_id);
    }

    fn load_pins(&self, install_id: u64) -> Option<Pins> {
        self.pins.get(&install_id).cloned()
    }

    fn save_pins(&mut self, pins: &Pins) -> Result<(), String> {
        self.pins.insert(pins.install_id, pins.clone());
        Ok(())
    }
}
