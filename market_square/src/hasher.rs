use std::hash::{BuildHasher, Hasher};

/// No-op hasher that returns the last [`u64`] value submitted as the hash.
#[derive(Default, Clone, Copy)]
pub struct IdentityHash(u64);

impl Hasher for IdentityHash {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        panic!("this hasher only takes u64.");
    }

    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
}

impl BuildHasher for IdentityHash {
    type Hasher = Self;

    fn build_hasher(&self) -> Self::Hasher {
        *self
    }
}

