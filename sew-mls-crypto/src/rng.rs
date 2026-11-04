//! The provider's RNG as the `rand_core` 0.9 RNG the `hpke` crate takes.
//!
//! The provider keeps `openmls_rust_crypto`'s generator (`rand_chacha` 0.3's `ChaCha20Rng`,
//! `rand_core` 0.6); `hpke` 0.13 is generic over `rand_core` 0.9's `CryptoRng + RngCore`. This
//! wrapper forwards one to the other, so HPKE's ephemeral keys come from the provider's RNG.

use hpke::rand_core as rc09;

/// A `rand_core` 0.6 CSPRNG seen as a `rand_core` 0.9 one.
pub(crate) struct Rng09<'a, R>(pub(crate) &'a mut R);

impl<R: rand_core::RngCore + rand_core::CryptoRng> rc09::RngCore for Rng09<'_, R> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.0.fill_bytes(dst);
    }
}

impl<R: rand_core::RngCore + rand_core::CryptoRng> rc09::CryptoRng for Rng09<'_, R> {}
