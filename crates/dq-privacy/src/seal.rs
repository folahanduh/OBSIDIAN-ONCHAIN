//! Sealed envelopes: one-shot ECIES to a static X25519 key with length padding.
//!
//! Used for (a) client → sequencer order flow (encrypted mempool) and
//! (b) user → auditor viewing-key disclosure. Each purpose has its own KDF
//! context, so a ciphertext produced for one can never be opened as the other.
//!
//! Wire: `epk(32) ‖ ct(k·BLOCK) ‖ tag(16)`, plaintext = `len_le32 ‖ payload ‖ 0-pad`.
//! Padding to fixed blocks hides payload length (e.g. order-type inference).

use core::fmt;

use chacha20poly1305::AeadInOut;
use rand_core::CryptoRng;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::{cipher, kdf, zero_nonce, PrivacyError, Result, CTX_SEAL_DISCLOSURE, CTX_SEAL_ORDER};

pub const SEAL_BLOCK: usize = 256;
pub const MAX_SEALED_LEN: usize = 64 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SealPurpose {
    Order,
    Disclosure,
}

impl SealPurpose {
    fn ctx(self) -> &'static str {
        match self {
            SealPurpose::Order => CTX_SEAL_ORDER,
            SealPurpose::Disclosure => CTX_SEAL_DISCLOSURE,
        }
    }
}

/// Static decryption key of a sequencer (or auditor). Production: held in an
/// HSM/TEE or replaced by a threshold key (see ARCHITECTURE.md §4.3).
pub struct SequencerKey(StaticSecret);

impl fmt::Debug for SequencerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SequencerKey(<redacted>)")
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SequencerPublicKey(pub [u8; 32]);

impl SequencerKey {
    pub fn generate<R: CryptoRng + ?Sized>(rng: &mut R) -> Self {
        SequencerKey(StaticSecret::random_from_rng(rng))
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        SequencerKey(StaticSecret::from(b))
    }

    pub fn public(&self) -> SequencerPublicKey {
        SequencerPublicKey(PublicKey::from(&self.0).to_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedEnvelope {
    pub epk: [u8; 32],
    pub ct: Vec<u8>,
    pub tag: [u8; 16],
}

impl SealedEnvelope {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(48 + self.ct.len());
        v.extend_from_slice(&self.epk);
        v.extend_from_slice(&self.ct);
        v.extend_from_slice(&self.tag);
        v
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let ct_len = b.len().checked_sub(48).ok_or(PrivacyError::Malformed)?;
        if ct_len == 0 || ct_len % SEAL_BLOCK != 0 || ct_len > MAX_SEALED_LEN {
            return Err(PrivacyError::Malformed);
        }
        let mut epk = [0u8; 32];
        let mut tag = [0u8; 16];
        epk.copy_from_slice(&b[..32]);
        tag.copy_from_slice(&b[32 + ct_len..]);
        Ok(SealedEnvelope {
            epk,
            ct: b[32..32 + ct_len].to_vec(),
            tag,
        })
    }
}

pub fn padded_len(payload_len: usize) -> usize {
    (payload_len + 4).div_ceil(SEAL_BLOCK) * SEAL_BLOCK
}

pub fn seal<R: CryptoRng + ?Sized>(
    purpose: SealPurpose,
    payload: &[u8],
    to: &SequencerPublicKey,
    rng: &mut R,
) -> Result<SealedEnvelope> {
    let len = padded_len(payload.len());
    if len > MAX_SEALED_LEN {
        return Err(PrivacyError::PayloadTooLarge);
    }
    let esk = EphemeralSecret::random_from_rng(rng);
    let epk = PublicKey::from(&esk).to_bytes();
    let shared = esk.diffie_hellman(&PublicKey::from(to.0));
    let key = kdf(purpose.ctx(), &shared, &epk, &to.0)?;

    let mut buf = vec![0u8; len];
    let n = u32::try_from(payload.len()).map_err(|_| PrivacyError::PayloadTooLarge)?;
    buf[..4].copy_from_slice(&n.to_le_bytes());
    buf[4..4 + payload.len()].copy_from_slice(payload);
    let tag = cipher(&key)
        .encrypt_inout_detached(&zero_nonce(), &epk, buf.as_mut_slice().into())
        .map_err(|_| PrivacyError::Malformed)?;
    Ok(SealedEnvelope {
        epk,
        ct: buf,
        tag: tag.into(),
    })
}

pub fn open(
    purpose: SealPurpose,
    env: &SealedEnvelope,
    key: &SequencerKey,
) -> Result<Zeroizing<Vec<u8>>> {
    if env.ct.is_empty() || env.ct.len() % SEAL_BLOCK != 0 || env.ct.len() > MAX_SEALED_LEN {
        return Err(PrivacyError::Malformed);
    }
    let rpk = key.public().0;
    let shared = key.0.diffie_hellman(&PublicKey::from(env.epk));
    let k = kdf(purpose.ctx(), &shared, &env.epk, &rpk)?;

    let mut buf = Zeroizing::new(env.ct.clone());
    cipher(&k)
        .decrypt_inout_detached(
            &zero_nonce(),
            &env.epk,
            buf.as_mut_slice().into(),
            &env.tag.into(),
        )
        .map_err(|_| PrivacyError::Decrypt)?;
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // Strict: declared length must fit and must produce exactly this padding.
    if n > buf.len() - 4 || padded_len(n) != buf.len() || buf[4 + n..].iter().any(|&b| b != 0) {
        return Err(PrivacyError::Malformed);
    }
    Ok(Zeroizing::new(buf[4..4 + n].to_vec()))
}
