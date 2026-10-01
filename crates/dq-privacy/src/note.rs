//! Encrypted fill notes.
//!
//! On-chain record per fill and counterparty:
//! `EncryptedNote { epoch, epk, cm, ct, tag }` where
//! - `cm  = BLAKE3-derive(CTX_NOTE_CM, encode(note))` — hiding because the note
//!   carries a 32-byte `rseed`; without it, low-entropy fields (price, qty)
//!   could be brute-forced from the commitment.
//! - `k   = KDF(X25519(esk, pk_epoch), epk, pk_epoch)`, one-time key.
//! - `ct  = ChaCha20-Poly1305(k, nonce=0, aad = epoch ‖ cm)`.
//!
//! Fixed-size plaintext ⇒ fixed-size ciphertext ⇒ no length side channel.

use dq_types::{AccountId, MarketId, Side};
use rand_core::CryptoRng;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::{Zeroize, Zeroizing};

use chacha20poly1305::AeadInOut;

use crate::keys::{EpochViewingKey, EpochViewingPublicKey};
use crate::{cipher, epoch_of, kdf, zero_nonce, PrivacyError, Result, CTX_NOTE_CM, CTX_NOTE_KDF};

const NOTE_VERSION: u8 = 1;
/// version(1) account(8) market(4) side(1) price(8) qty(8) fee(16) fill_seq(8) ts(8) rseed(32)
pub const NOTE_LEN: usize = 1 + 8 + 4 + 1 + 8 + 8 + 16 + 8 + 8 + 32;

#[derive(Clone, Debug, PartialEq, Eq, Zeroize)]
pub struct FillNote {
    pub account: AccountId,
    pub market: MarketId,
    #[zeroize(skip)]
    pub side: Side,
    pub price: u64,
    pub qty: u64,
    /// Quote atoms: positive = fee paid, negative = rebate received.
    pub fee: i128,
    pub fill_seq: u64,
    pub ts_ms: u64,
    pub rseed: [u8; 32],
}

impl FillNote {
    pub fn encode(&self) -> [u8; NOTE_LEN] {
        let mut b = [0u8; NOTE_LEN];
        let mut w = Writer {
            buf: &mut b,
            pos: 0,
        };
        w.put(&[NOTE_VERSION]);
        w.put(&self.account.to_le_bytes());
        w.put(&self.market.to_le_bytes());
        w.put(&[self.side as u8]);
        w.put(&self.price.to_le_bytes());
        w.put(&self.qty.to_le_bytes());
        w.put(&self.fee.to_le_bytes());
        w.put(&self.fill_seq.to_le_bytes());
        w.put(&self.ts_ms.to_le_bytes());
        w.put(&self.rseed);
        debug_assert_eq!(w.pos, NOTE_LEN);
        b
    }

    pub fn decode(b: &[u8; NOTE_LEN]) -> Result<Self> {
        let mut r = Reader { buf: b, pos: 0 };
        if r.take::<1>()[0] != NOTE_VERSION {
            return Err(PrivacyError::Malformed);
        }
        let account = u64::from_le_bytes(r.take());
        let market = u32::from_le_bytes(r.take());
        let side = Side::from_u8(r.take::<1>()[0]).ok_or(PrivacyError::Malformed)?;
        Ok(FillNote {
            account,
            market,
            side,
            price: u64::from_le_bytes(r.take()),
            qty: u64::from_le_bytes(r.take()),
            fee: i128::from_le_bytes(r.take()),
            fill_seq: u64::from_le_bytes(r.take()),
            ts_ms: u64::from_le_bytes(r.take()),
            rseed: r.take(),
        })
    }

    pub fn commitment(&self) -> [u8; 32] {
        let pt = Zeroizing::new(self.encode());
        *blake3::Hasher::new_derive_key(CTX_NOTE_CM)
            .update(pt.as_ref())
            .finalize()
            .as_bytes()
    }
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}
impl Writer<'_> {
    fn put(&mut self, s: &[u8]) {
        self.buf[self.pos..self.pos + s.len()].copy_from_slice(s);
        self.pos += s.len();
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedNote {
    pub epoch: u64,
    pub epk: [u8; 32],
    pub cm: [u8; 32],
    pub ct: [u8; NOTE_LEN],
    pub tag: [u8; 16],
}

fn aad(epoch: u64, cm: &[u8; 32]) -> [u8; 40] {
    let mut a = [0u8; 40];
    a[..8].copy_from_slice(&epoch.to_le_bytes());
    a[8..].copy_from_slice(cm);
    a
}

/// Encrypt `note` to the owner's registered epoch key. The note's timestamp
/// must fall inside that epoch, so disclosure scoping is enforced by
/// construction.
pub fn encrypt_note<R: CryptoRng + ?Sized>(
    note: &FillNote,
    to: &EpochViewingPublicKey,
    rng: &mut R,
) -> Result<EncryptedNote> {
    if note.account != to.account {
        return Err(PrivacyError::WrongAccount);
    }
    if epoch_of(note.ts_ms) != to.epoch {
        return Err(PrivacyError::WrongEpoch);
    }
    let esk = EphemeralSecret::random_from_rng(rng);
    let epk = PublicKey::from(&esk).to_bytes();
    let shared = esk.diffie_hellman(&PublicKey::from(to.pk));
    let key = kdf(CTX_NOTE_KDF, &shared, &epk, &to.pk)?;

    let cm = note.commitment();
    let mut ct = note.encode();
    let tag = cipher(&key)
        .encrypt_inout_detached(&zero_nonce(), &aad(to.epoch, &cm), ct.as_mut_slice().into())
        .map_err(|_| PrivacyError::Malformed)?;
    Ok(EncryptedNote {
        epoch: to.epoch,
        epk,
        cm,
        ct,
        tag: tag.into(),
    })
}

pub fn decrypt_note(enc: &EncryptedNote, key: &EpochViewingKey) -> Result<FillNote> {
    if enc.epoch != key.epoch() {
        return Err(PrivacyError::WrongEpoch);
    }
    let rpk = key.public().pk;
    let shared = key.secret().diffie_hellman(&PublicKey::from(enc.epk));
    let k = kdf(CTX_NOTE_KDF, &shared, &enc.epk, &rpk)?;

    let mut pt = Zeroizing::new(enc.ct);
    cipher(&k)
        .decrypt_inout_detached(
            &zero_nonce(),
            &aad(enc.epoch, &enc.cm),
            pt.as_mut_slice().into(),
            &enc.tag.into(),
        )
        .map_err(|_| PrivacyError::Decrypt)?;
    let note = FillNote::decode(&pt)?;
    if note.account != key.account() {
        return Err(PrivacyError::WrongAccount);
    }
    if epoch_of(note.ts_ms) != enc.epoch {
        return Err(PrivacyError::WrongEpoch);
    }
    if !bool::from(note.commitment().ct_eq(&enc.cm)) {
        return Err(PrivacyError::CommitmentMismatch);
    }
    Ok(note)
}

pub(crate) fn try_decrypt_note(enc: &EncryptedNote, key: &EpochViewingKey) -> Option<FillNote> {
    decrypt_note(enc, key).ok()
}
