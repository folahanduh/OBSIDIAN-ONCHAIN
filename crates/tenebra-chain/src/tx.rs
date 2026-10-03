//! Transactions: canonical encoding, Ed25519 signing, strict decoding.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::types::{Address, Amount, AssetId, TxError};

pub const DOMAIN_TX: &[u8] = b"Tenebra/v1/tx\0";

/// Gas units per transaction kind. Fees are `gas × base_fee`, paid in the
/// native coin and burned.
pub mod gas {
    pub const TRANSFER: u64 = 1_000;
    pub const REGISTER_VALIDATOR: u64 = 20_000;
    pub const DELEGATE: u64 = 5_000;
    pub const UNDELEGATE: u64 = 5_000;
    pub const CLAIM_REWARDS: u64 = 3_000;
    pub const UNJAIL: u64 = 3_000;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxKind {
    Transfer {
        to: Address,
        asset: AssetId,
        amount: Amount,
    },
    /// Sender becomes the operator; `self_bond` is moved from its balance.
    RegisterValidator {
        consensus_key: [u8; 32],
        commission_bps: u32,
        self_bond: Amount,
    },
    Delegate {
        validator: Address,
        amount: Amount,
    },
    /// Starts unbonding `amount` worth of stake; paid out after the
    /// unbonding period (and slashable until then).
    Undelegate {
        validator: Address,
        amount: Amount,
    },
    ClaimRewards {
        validator: Address,
    },
    Unjail,
}

impl TxKind {
    pub fn gas(&self) -> u64 {
        match self {
            TxKind::Transfer { .. } => gas::TRANSFER,
            TxKind::RegisterValidator { .. } => gas::REGISTER_VALIDATOR,
            TxKind::Delegate { .. } => gas::DELEGATE,
            TxKind::Undelegate { .. } => gas::UNDELEGATE,
            TxKind::ClaimRewards { .. } => gas::CLAIM_REWARDS,
            TxKind::Unjail => gas::UNJAIL,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tx {
    pub chain_id: u64,
    pub sender: Address,
    pub nonce: u64,
    /// Upper bound the sender accepts; the charged price is the block's base fee.
    pub max_fee_per_gas: Amount,
    pub kind: TxKind,
    pub sig: [u8; 64],
}

#[derive(Default)]
struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u128(&mut self, v: u128) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn b32(&mut self, v: &[u8; 32]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct R<'a>(&'a [u8]);
impl R<'_> {
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], TxError> {
        if self.0.len() < N {
            return Err(TxError::Malformed);
        }
        let (h, t) = self.0.split_at(N);
        self.0 = t;
        h.try_into().map_err(|_| TxError::Malformed)
    }
    fn u8(&mut self) -> Result<u8, TxError> {
        Ok(self.arr::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, TxError> {
        self.arr().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, TxError> {
        self.arr().map(u64::from_le_bytes)
    }
    fn u128(&mut self) -> Result<u128, TxError> {
        self.arr().map(u128::from_le_bytes)
    }
}

impl Tx {
    fn body(&self, w: &mut W) {
        w.u64(self.chain_id)
            .b32(&self.sender.0)
            .u64(self.nonce)
            .u128(self.max_fee_per_gas);
        match &self.kind {
            TxKind::Transfer { to, asset, amount } => {
                w.u8(0).b32(&to.0).u32(*asset).u128(*amount);
            }
            TxKind::RegisterValidator {
                consensus_key,
                commission_bps,
                self_bond,
            } => {
                w.u8(1)
                    .b32(consensus_key)
                    .u32(*commission_bps)
                    .u128(*self_bond);
            }
            TxKind::Delegate { validator, amount } => {
                w.u8(2).b32(&validator.0).u128(*amount);
            }
            TxKind::Undelegate { validator, amount } => {
                w.u8(3).b32(&validator.0).u128(*amount);
            }
            TxKind::ClaimRewards { validator } => {
                w.u8(4).b32(&validator.0);
            }
            TxKind::Unjail => {
                w.u8(5);
            }
        }
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.0.extend_from_slice(DOMAIN_TX);
        self.body(&mut w);
        w.0
    }

    pub fn signed(
        chain_id: u64,
        key: &SigningKey,
        nonce: u64,
        max_fee_per_gas: Amount,
        kind: TxKind,
    ) -> Tx {
        let mut tx = Tx {
            chain_id,
            sender: Address(key.verifying_key().to_bytes()),
            nonce,
            max_fee_per_gas,
            kind,
            sig: [0; 64],
        };
        tx.sig = key.sign(&tx.signing_bytes()).to_bytes();
        tx
    }

    /// Strict Ed25519 verification (rejects malleable signatures and
    /// small-order keys).
    pub fn verify(&self) -> Result<(), TxError> {
        let key = VerifyingKey::from_bytes(&self.sender.0).map_err(|_| TxError::BadSignature)?;
        if key.is_weak() {
            return Err(TxError::BadSignature);
        }
        key.verify_strict(&self.signing_bytes(), &Signature::from_bytes(&self.sig))
            .map_err(|_| TxError::BadSignature)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        self.body(&mut w);
        w.0.extend_from_slice(&self.sig);
        w.0
    }

    pub fn decode(b: &[u8]) -> Result<Tx, TxError> {
        let mut r = R(b);
        let chain_id = r.u64()?;
        let sender = Address(r.arr()?);
        let nonce = r.u64()?;
        let max_fee_per_gas = r.u128()?;
        let kind = match r.u8()? {
            0 => TxKind::Transfer {
                to: Address(r.arr()?),
                asset: r.u32()?,
                amount: r.u128()?,
            },
            1 => TxKind::RegisterValidator {
                consensus_key: r.arr()?,
                commission_bps: r.u32()?,
                self_bond: r.u128()?,
            },
            2 => TxKind::Delegate {
                validator: Address(r.arr()?),
                amount: r.u128()?,
            },
            3 => TxKind::Undelegate {
                validator: Address(r.arr()?),
                amount: r.u128()?,
            },
            4 => TxKind::ClaimRewards {
                validator: Address(r.arr()?),
            },
            5 => TxKind::Unjail,
            _ => return Err(TxError::Malformed),
        };
        let sig = r.arr()?;
        if !r.0.is_empty() {
            return Err(TxError::Malformed);
        }
        Ok(Tx {
            chain_id,
            sender,
            nonce,
            max_fee_per_gas,
            kind,
            sig,
        })
    }
}
