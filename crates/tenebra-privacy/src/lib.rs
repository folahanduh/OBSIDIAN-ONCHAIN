//! Selective-disclosure privacy layer.
//!
//! Three primitives, all built from X25519 + BLAKE3 (KDF/commitments) +
//! ChaCha20-Poly1305:
//!
//! 1. **Epoch viewing keys** ([`keys`]): a per-account `ViewingSeed` derives an
//!    independent X25519 key per epoch (hardened derivation — no epoch key
//!    reveals the seed or any other epoch). The user pre-registers epoch public
//!    keys; disclosing a *range* of epoch secrets to a CEX gives a read-only,
//!    time-bounded audit capability. Viewing keys cannot sign: spend/trade
//!    authority lives in `tenebra-auth` under unrelated Ed25519 keys.
//! 2. **Encrypted fill notes** ([`note`]): every fill is published as a hiding
//!    commitment plus a ciphertext readable only by the epoch viewing key.
//!    Decryption re-derives the commitment, which makes the scheme
//!    key-committing (an auditor cannot be shown a different plaintext than
//!    the one committed on-chain).
//! 3. **Sealed orders + ordering log** ([`seal`], [`ordering`]): clients encrypt
//!    signed actions to the sequencer key with length padding; the sequencer
//!    commits to their order in a hash chain *before* decrypting, so it cannot
//!    reorder on content (front-running) without producing equivocating
//!    receipts.

#![forbid(unsafe_code)]

pub mod keys;
pub mod note;
pub mod ordering;
pub mod seal;

pub use keys::{ComplianceDisclosure, EpochViewingKey, EpochViewingPublicKey, ViewingSeed};
pub use note::{decrypt_note, encrypt_note, EncryptedNote, FillNote, NOTE_LEN};
pub use ordering::{OrderingLog, SequenceReceipt};
pub use seal::{open, seal, SealPurpose, SealedEnvelope, SequencerKey, SequencerPublicKey};

/// Epoch length for viewing-key rotation (1 day).
pub const EPOCH_MS: u64 = 86_400_000;

#[inline]
pub const fn epoch_of(ts_ms: u64) -> u64 {
    ts_ms / EPOCH_MS
}

// BLAKE3 derive_key contexts: globally unique, hardcoded, never reused.
pub(crate) const CTX_EPOCH_IVK: &str = "Tenebra 2026-10-03 epoch viewing key v1";
pub(crate) const CTX_NOTE_KDF: &str = "Tenebra 2026-10-03 note encryption key v1";
pub(crate) const CTX_NOTE_CM: &str = "Tenebra 2026-10-03 note commitment v1";
pub(crate) const CTX_SEAL_ORDER: &str = "Tenebra 2026-10-03 sealed order key v1";
pub(crate) const CTX_SEAL_DISCLOSURE: &str = "Tenebra 2026-10-03 sealed disclosure key v1";
pub(crate) const CTX_SEQ_LEAF: &str = "Tenebra 2026-10-03 ordering log leaf v1";
pub(crate) const CTX_SEQ_CHAIN: &str = "Tenebra 2026-10-03 ordering log chain v1";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PrivacyError {
    /// DH output was all-zero (low-order peer key). Never encrypt to it.
    NonContributory,
    /// AEAD authentication failed (wrong key or tampered ciphertext).
    Decrypt,
    Malformed,
    WrongEpoch,
    WrongAccount,
    /// Ciphertext decrypted but does not open the published commitment.
    CommitmentMismatch,
    PayloadTooLarge,
}

pub type Result<T> = core::result::Result<T, PrivacyError>;

/// ECIES-style key agreement shared by notes and sealed envelopes:
/// `k = BLAKE3-derive(ctx, dh || epk || recipient_pk)`. Binding both public
/// keys into the KDF prevents key-reuse/unknown-key-share issues (as in HPKE).
pub(crate) fn kdf(
    ctx: &str,
    shared: &x25519_dalek::SharedSecret,
    epk: &[u8; 32],
    rpk: &[u8; 32],
) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    if !shared.was_contributory() {
        return Err(PrivacyError::NonContributory);
    }
    let mut h = blake3::Hasher::new_derive_key(ctx);
    h.update(shared.as_bytes());
    h.update(epk);
    h.update(rpk);
    Ok(zeroize::Zeroizing::new(*h.finalize().as_bytes()))
}

pub(crate) fn cipher(key: &[u8; 32]) -> chacha20poly1305::ChaCha20Poly1305 {
    use chacha20poly1305::KeyInit;
    chacha20poly1305::ChaCha20Poly1305::new(&chacha20poly1305::Key::from(*key))
}

/// Every AEAD key here is derived from a fresh ephemeral DH and used for
/// exactly one message, so a constant nonce is safe (as in Sapling notes).
pub(crate) fn zero_nonce() -> chacha20poly1305::Nonce {
    chacha20poly1305::Nonce::default()
}
