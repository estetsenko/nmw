//! Node identities and addresses.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as DhPublic, StaticSecret};

/// Full 16-byte node address: `SHA-256(signing_pub)[..16]`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Address(pub [u8; 16]);

impl Address {
    /// The 4-byte on-air form of this address.
    pub fn short(&self) -> ShortAddr {
        ShortAddr([self.0[0], self.0[1], self.0[2], self.0[3]])
    }
}

/// 4-byte on-air address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ShortAddr(pub [u8; 4]);

impl ShortAddr {
    pub const BROADCAST: ShortAddr = ShortAddr([0xff; 4]);

    /// 1-byte repeater tag used in paths.
    pub fn tag(&self) -> u8 {
        self.0[0]
    }
}

/// The public half of an identity, as carried in announces: one Ed25519 key.
/// The X25519 key used for key agreement is derived from it (as libsodium's
/// `crypto_sign_ed25519_pk_to_curve25519`), which saves 32 bytes per announce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublicIdentity {
    signing: VerifyingKey,
}

impl PublicIdentity {
    pub const LEN: usize = 32;

    pub fn address(&self) -> Address {
        let digest = Sha256::digest(self.signing.as_bytes());
        let mut a = [0u8; 16];
        a.copy_from_slice(&digest[..16]);
        Address(a)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Rejects keys that are not valid points or have small order.
    pub fn from_bytes(b: &[u8; 32]) -> Option<Self> {
        let signing = VerifyingKey::from_bytes(b).ok()?;
        if signing.is_weak() {
            return None;
        }
        Some(PublicIdentity { signing })
    }

    fn dh(&self) -> DhPublic {
        DhPublic::from(self.signing.to_montgomery().to_bytes())
    }

    /// Verify an Ed25519 signature made by this identity.
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        self.signing
            .verify_strict(msg, &Signature::from_bytes(sig))
            .is_ok()
    }
}

/// A node's private identity.
pub struct Identity {
    signing: SigningKey,
    dh: StaticSecret,
    public: PublicIdentity,
}

impl Identity {
    /// Derive an identity from a 32-byte seed (draw it from a hardware RNG).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let dh = StaticSecret::from(signing.to_scalar_bytes());
        let public = PublicIdentity {
            signing: signing.verifying_key(),
        };
        Identity {
            signing,
            dh,
            public,
        }
    }

    pub fn public(&self) -> &PublicIdentity {
        &self.public
    }

    pub fn address(&self) -> Address {
        self.public.address()
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// Symmetric key shared with `peer`: X25519, then HKDF bound to both addresses.
    /// Returns `None` for a non-contributory result.
    pub fn pair_key(&self, peer: &PublicIdentity) -> Option<[u8; 32]> {
        let shared = self.dh.diffie_hellman(&peer.dh());
        if !shared.was_contributory() {
            return None;
        }
        let (a, b) = (self.address(), peer.address());
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let mut info = [0u8; 32];
        info[..16].copy_from_slice(&lo.0);
        info[16..].copy_from_slice(&hi.0);
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(Some(b"nmw-v0 pair"), shared.as_bytes())
            .expand(&info, &mut key)
            .expect("32 bytes is a valid HKDF output length");
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_keys_match() {
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        assert_eq!(a.pair_key(b.public()), b.pair_key(a.public()));
        assert_ne!(a.address(), b.address());
    }

    #[test]
    fn public_identity_roundtrip_and_signature() {
        let a = Identity::from_seed(&[7; 32]);
        let p = PublicIdentity::from_bytes(&a.public().to_bytes()).unwrap();
        assert_eq!(p.address(), a.address());
        let sig = a.sign(b"hello");
        assert!(p.verify(b"hello", &sig));
        assert!(!p.verify(b"hellp", &sig));
    }
}
