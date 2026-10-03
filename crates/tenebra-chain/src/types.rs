//! Core types and chain parameters.

use core::fmt;

/// Native-coin and token amounts in base units.
pub type Amount = u128;
pub type AssetId = u32;

/// The native gas and staking coin.
pub const NATIVE: AssetId = 0;
pub const BPS: u128 = 10_000;
/// 365.25 days.
pub const SECONDS_PER_YEAR: u128 = 31_557_600;

/// An account address: the account's Ed25519 public key.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 32]);

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address(")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainParams {
    pub chain_id: u64,
    /// Display only, e.g. "TENEBERA". Never used in consensus logic.
    pub native_symbol: String,
    pub native_decimals: u8,

    // ---- gas / fee market (EIP-1559-style base fee, 100% burned)
    pub block_gas_limit: u64,
    pub block_gas_target: u64,
    pub initial_base_fee: Amount,
    pub min_base_fee: Amount,
    /// Max base-fee change per block = 1/denominator (8 ⇒ ±12.5%).
    pub base_fee_change_denominator: u128,

    // ---- staking
    pub max_validators: u32,
    pub min_self_bond: Amount,
    pub max_commission_bps: u32,
    pub unbonding_secs: i64,
    /// Base units of stake per unit of consensus voting power.
    pub power_reduction: Amount,

    // ---- issuance (paid to validators that signed the previous block)
    pub initial_issuance_bps: u32,
    pub issuance_floor_bps: u32,
    /// Linear decay of the issuance rate per year since genesis.
    pub issuance_decay_bps_per_year: u32,
    /// Longest block interval credited with issuance (guards against
    /// timestamp jumps after a halt).
    pub max_issuance_interval_secs: i64,

    // ---- slashing
    pub slash_double_sign_bps: u32,
    pub slash_downtime_bps: u32,
    pub downtime_window_blocks: u64,
    pub max_missed_in_window: u64,
    pub downtime_jail_secs: i64,
}

impl ChainParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.block_gas_target == 0 || self.block_gas_target > self.block_gas_limit {
            return Err("gas target must be in (0, limit]");
        }
        if self.min_base_fee == 0 || self.initial_base_fee < self.min_base_fee {
            return Err("base fee must be >= min_base_fee > 0");
        }
        if self.base_fee_change_denominator == 0 {
            return Err("base fee change denominator is zero");
        }
        if self.max_validators == 0 || self.power_reduction == 0 {
            return Err("validator set parameters must be non-zero");
        }
        if self.max_commission_bps as u128 > BPS
            || self.slash_double_sign_bps as u128 > BPS
            || self.slash_downtime_bps as u128 > BPS
            || self.issuance_floor_bps > self.initial_issuance_bps
        {
            return Err("basis-point parameter out of range");
        }
        if self.unbonding_secs <= 0
            || self.downtime_window_blocks == 0
            || self.max_issuance_interval_secs <= 0
        {
            return Err("periods must be positive");
        }
        Ok(())
    }
}

/// Why a transaction was rejected (not included) or failed (included, fee
/// charged, effects reverted).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TxError {
    // ---- rejections: the transaction is invalid and is not charged
    Malformed,
    WrongChain,
    BadSignature,
    BadNonce { expected: u64, got: u64 },
    FeeCapBelowBaseFee,
    BlockGasExceeded,
    CannotPayFee,
    // ---- failures: fee is burned, effects are reverted
    InsufficientBalance,
    ZeroAmount,
    UnknownAsset,
    UnknownValidator,
    ValidatorExists,
    InvalidConsensusKey,
    ConsensusKeyInUse,
    CommissionTooHigh,
    SelfBondTooLow,
    InsufficientDelegation,
    ValidatorFullySlashed,
    NothingToClaim,
    NotJailed,
    StillJailed,
    Tombstoned,
    Overflow,
}

impl TxError {
    /// Rejected transactions never enter a block and pay nothing.
    pub fn is_rejection(&self) -> bool {
        matches!(
            self,
            TxError::Malformed
                | TxError::WrongChain
                | TxError::BadSignature
                | TxError::BadNonce { .. }
                | TxError::FeeCapBelowBaseFee
                | TxError::BlockGasExceeded
                | TxError::CannotPayFee
        )
    }
}
