//! The provider's RNG as the `rand_core` 0.10 RNG the `hpke` crate takes.
//!
//! The provider keeps `openmls_rust_crypto`'s generator (`rand_chacha` 0.3's `ChaCha20Rng`,
//! `rand_core` 0.6); `hpke` 0.15 is generic over `rand_core` 0.10's `CryptoRng`. This wrapper
//! forwards one to the other, so HPKE's ephemeral keys come from the provider's RNG.

use core::convert::Infallible;

use hpke::rand_core as rc10;

/// A `rand_core` 0.6 CSPRNG seen as a `rand_core` 0.10 one.
pub(crate) struct Rng10<'a, R>(pub(crate) &'a mut R);

impl<R: rand_core::RngCore + rand_core::CryptoRng> rc10::TryRng for Rng10<'_, R> {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(self.0.next_u32())
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(self.0.next_u64())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        self.0.fill_bytes(dst);
        Ok(())
    }
}

impl<R: rand_core::RngCore + rand_core::CryptoRng> rc10::TryCryptoRng for Rng10<'_, R> {}
