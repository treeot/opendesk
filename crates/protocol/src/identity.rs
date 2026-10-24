//! Persistent device identity: an Ed25519 keypair that proves which device is
//! connecting, independently of the spoofable relay ID.
//!
//! After SPAKE2 completes, the viewer signs a value derived from the session's
//! shared secret with its device key and sends the public key plus signature.
//! The host verifies it. Because the signed value depends on the shared secret
//! (which needs the password), the signature cannot be replayed or forged by a
//! relay or a spoofer, so the host can trust the public key's fingerprint for
//! allow-listing and trusted-device decisions.
//!
//! No randomness is generated here, so this stays usable on wasm: the caller
//! provides the 32-byte seed (generated with its own RNG) and stores it.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// The binding message both sides compute from the live session's shared
/// secret. Signing it ties the device key to this specific session.
fn bind_value(shared_secret: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, shared_secret)
        .expand(b"opendesk device-bind", &mut out)
        .expect("32 bytes fits");
    out
}

/// A device's long-term signing key. The 32-byte seed is the secret to store
/// (in the OS keyring); everything else derives from it.
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    /// The secret seed, for storing in the keyring.
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The public key bytes shared with peers.
    pub fn public(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    /// Sign this session's binding value, proving possession of the key.
    pub fn sign_binding(&self, shared_secret: &[u8]) -> [u8; 64] {
        self.0.sign(&bind_value(shared_secret)).to_bytes()
    }
}

/// Verify that `signature` over this session's binding was made by the holder
/// of `public`. False on any malformed input or mismatch.
pub fn verify_binding(public: &[u8; 32], shared_secret: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else { return false };
    let sig = Signature::from_bytes(signature);
    key.verify(&bind_value(shared_secret), &sig).is_ok()
}

/// A short, human-comparable fingerprint of a public key: the first 128 bits of
/// its SHA-256, uppercase hex grouped in fours (e.g. "A1B2 C3D4 ...").
pub fn fingerprint(public: &[u8; 32]) -> String {
    let digest = Sha256::digest(public);
    let hex: String = digest[..8].iter().map(|byte| format!("{byte:02X}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_A: [u8; 32] = [1u8; 32];
    const SEED_B: [u8; 32] = [2u8; 32];

    #[test]
    fn a_valid_binding_verifies() {
        let key = DeviceKey::from_seed(&SEED_A);
        let secret = [9u8; 32];
        let sig = key.sign_binding(&secret);
        assert!(verify_binding(&key.public(), &secret, &sig));
    }

    #[test]
    fn a_binding_for_another_session_fails() {
        let key = DeviceKey::from_seed(&SEED_A);
        let sig = key.sign_binding(&[9u8; 32]);
        // A spoofer replaying the signature into a different session (different
        // shared secret) is rejected.
        assert!(!verify_binding(&key.public(), &[8u8; 32], &sig));
    }

    #[test]
    fn another_devices_key_cannot_claim_the_binding() {
        let key = DeviceKey::from_seed(&SEED_A);
        let other = DeviceKey::from_seed(&SEED_B);
        let secret = [9u8; 32];
        let sig = key.sign_binding(&secret);
        // The signature does not verify against a different device's public key.
        assert!(!verify_binding(&other.public(), &secret, &sig));
    }

    #[test]
    fn the_seed_round_trips() {
        let key = DeviceKey::from_seed(&SEED_A);
        assert_eq!(key.seed(), SEED_A);
        assert_eq!(DeviceKey::from_seed(&key.seed()).public(), key.public());
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        let a = DeviceKey::from_seed(&SEED_A).public();
        let b = DeviceKey::from_seed(&SEED_B).public();
        assert_eq!(fingerprint(&a), fingerprint(&a));
        assert_ne!(fingerprint(&a), fingerprint(&b));
        // 8 bytes -> 16 hex chars -> four groups of four separated by spaces.
        assert_eq!(fingerprint(&a).len(), 19);
    }

    #[test]
    fn malformed_signature_is_rejected() {
        let key = DeviceKey::from_seed(&SEED_A);
        assert!(!verify_binding(&key.public(), &[9u8; 32], &[0u8; 64]));
    }
}
