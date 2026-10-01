//! Viewing-key hierarchy.
//!
//! ```text
//! ViewingSeed (32B, user-held)
//!   └─ BLAKE3-derive(CTX_EPOCH_IVK, seed ‖ account ‖ epoch) → X25519 secret  (hardened)
//!        ├─ EpochViewingPublicKey  → registered on-chain ahead of time
//!        └─ EpochViewingKey        → disclosed to auditors per epoch range
//! ```
//!
//! Hardened derivation is deliberate. Non-hardened (BIP32-style additive)
//! derivation would let senders compute future public keys without
//! registration, but any disclosed child secret plus the master public key
//! reveals the master secret — i.e. a one-day disclosure would leak all
//! history. Pre-registration is the price of bounded disclosure.

use core::fmt;

use dq_types::AccountId;
use rand_core::CryptoRng;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::note::{try_decrypt_note, EncryptedNote, FillNote};
use crate::{PrivacyError, Result, CTX_EPOCH_IVK};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct ViewingSeed([u8; 32]);

impl fmt::Debug for ViewingSeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ViewingSeed(<redacted>)")
    }
}

impl ViewingSeed {
    pub fn generate<R: CryptoRng + ?Sized>(rng: &mut R) -> Self {
        let mut b = [0u8; 32];
        rng.fill_bytes(&mut b);
        ViewingSeed(b)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ViewingSeed(bytes)
    }

    pub fn epoch_key(&self, account: AccountId, epoch: u64) -> EpochViewingKey {
        let mut h = blake3::Hasher::new_derive_key(CTX_EPOCH_IVK);
        h.update(&self.0);
        h.update(&account.to_le_bytes());
        h.update(&epoch.to_le_bytes());
        let sk = Zeroizing::new(*h.finalize().as_bytes());
        EpochViewingKey {
            account,
            epoch,
            secret: StaticSecret::from(*sk),
        }
    }
}

/// Read-only decryption capability for one account over one epoch.
pub struct EpochViewingKey {
    account: AccountId,
    epoch: u64,
    secret: StaticSecret, // zeroized on drop by x25519-dalek
}

impl fmt::Debug for EpochViewingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EpochViewingKey")
            .field("account", &self.account)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl EpochViewingKey {
    pub fn account(&self) -> AccountId {
        self.account
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn public(&self) -> EpochViewingPublicKey {
        EpochViewingPublicKey {
            account: self.account,
            epoch: self.epoch,
            pk: PublicKey::from(&self.secret).to_bytes(),
        }
    }

    pub(crate) fn secret(&self) -> &StaticSecret {
        &self.secret
    }

    /// Raw secret for disclosure. Only ever transmit via `seal(SealPurpose::Disclosure, ..)`.
    pub fn export_secret(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    pub fn from_secret(account: AccountId, epoch: u64, secret: [u8; 32]) -> Self {
        EpochViewingKey {
            account,
            epoch,
            secret: StaticSecret::from(secret),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EpochViewingPublicKey {
    pub account: AccountId,
    pub epoch: u64,
    pub pk: [u8; 32],
}

impl EpochViewingPublicKey {
    /// Reject low-order points at registration time. A clamped X25519 scalar
    /// is a multiple of the cofactor, so DH with any small-order point is
    /// all-zero for *every* scalar; probing with a fixed scalar suffices.
    pub fn is_valid(&self) -> bool {
        StaticSecret::from([0x42; 32])
            .diffie_hellman(&PublicKey::from(self.pk))
            .was_contributory()
    }
}

/// A set of epoch viewing keys handed to an auditor (e.g. a CEX compliance
/// desk) for one account. Grants read access to exactly those epochs.
#[derive(Debug)]
pub struct ComplianceDisclosure {
    account: AccountId,
    keys: Vec<EpochViewingKey>, // sorted by epoch, unique
}

const DISCLOSURE_MAGIC: &[u8; 8] = b"DQDISCv1";
/// Bound on epochs per disclosure (≈ 3 years of daily keys).
pub const MAX_DISCLOSED_EPOCHS: usize = 1_100;

impl ComplianceDisclosure {
    pub fn new(account: AccountId, mut keys: Vec<EpochViewingKey>) -> Result<Self> {
        if keys.len() > MAX_DISCLOSED_EPOCHS {
            return Err(PrivacyError::PayloadTooLarge);
        }
        if keys.iter().any(|k| k.account != account) {
            return Err(PrivacyError::WrongAccount);
        }
        keys.sort_by_key(|k| k.epoch);
        if keys.windows(2).any(|w| w[0].epoch == w[1].epoch) {
            return Err(PrivacyError::Malformed);
        }
        Ok(ComplianceDisclosure { account, keys })
    }

    /// Disclose the contiguous epoch range `[from, to]` from a seed.
    pub fn from_seed_range(
        seed: &ViewingSeed,
        account: AccountId,
        from: u64,
        to: u64,
    ) -> Result<Self> {
        if to < from || to - from >= MAX_DISCLOSED_EPOCHS as u64 {
            return Err(PrivacyError::PayloadTooLarge);
        }
        Self::new(
            account,
            (from..=to).map(|e| seed.epoch_key(account, e)).collect(),
        )
    }

    pub fn account(&self) -> AccountId {
        self.account
    }

    pub fn epochs(&self) -> impl Iterator<Item = u64> + '_ {
        self.keys.iter().map(|k| k.epoch)
    }

    fn key_for(&self, epoch: u64) -> Option<&EpochViewingKey> {
        self.keys
            .binary_search_by_key(&epoch, |k| k.epoch)
            .ok()
            .map(|i| &self.keys[i])
    }

    /// Trial-decrypt a stream of on-chain notes; returns the account's fills
    /// inside the disclosed epochs. Notes outside the range are skipped
    /// without any cryptographic work.
    pub fn scan<'a>(&self, notes: impl IntoIterator<Item = &'a EncryptedNote>) -> Vec<FillNote> {
        notes
            .into_iter()
            .filter_map(|n| try_decrypt_note(n, self.key_for(n.epoch)?))
            .collect()
    }

    /// `magic ‖ account ‖ n ‖ (epoch ‖ secret)*`. Contains secrets: seal it.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut v = Zeroizing::new(Vec::with_capacity(20 + self.keys.len() * 40));
        v.extend_from_slice(DISCLOSURE_MAGIC);
        v.extend_from_slice(&self.account.to_le_bytes());
        v.extend_from_slice(
            &u32::try_from(self.keys.len())
                .expect("bounded by MAX_DISCLOSED_EPOCHS")
                .to_le_bytes(),
        );
        for k in &self.keys {
            v.extend_from_slice(&k.epoch.to_le_bytes());
            v.extend_from_slice(k.export_secret().as_ref());
        }
        v
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let rd8 = |s: &[u8]| -> Result<u64> {
            Ok(u64::from_le_bytes(
                s.try_into().map_err(|_| PrivacyError::Malformed)?,
            ))
        };
        if b.len() < 20 || &b[..8] != DISCLOSURE_MAGIC {
            return Err(PrivacyError::Malformed);
        }
        let account = rd8(&b[8..16])?;
        let n =
            u32::from_le_bytes(b[16..20].try_into().map_err(|_| PrivacyError::Malformed)?) as usize;
        if n > MAX_DISCLOSED_EPOCHS || b.len() != 20 + n * 40 {
            return Err(PrivacyError::Malformed);
        }
        let keys = b[20..]
            .chunks_exact(40)
            .map(|c| {
                let mut sk = Zeroizing::new([0u8; 32]);
                sk.copy_from_slice(&c[8..]);
                Ok(EpochViewingKey::from_secret(account, rd8(&c[..8])?, *sk))
            })
            .collect::<Result<Vec<_>>>()?;
        Self::new(account, keys)
    }
}
