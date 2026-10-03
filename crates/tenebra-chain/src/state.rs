//! The chain's state machine.
//!
//! Block lifecycle (maps 1:1 onto CometBFT ABCI 2.0 `FinalizeBlock` + `Commit`):
//!
//! ```text
//! begin_block  → slash double-signers (evidence), track downtime,
//!                mint issuance to validators that signed the previous block
//! deliver_tx*  → charge gas in the native coin and BURN it, then execute
//! end_block    → pay out matured unbondings, adjust the base fee,
//!                recompute the validator set (returned to consensus)
//! commit       → deterministic app hash
//! ```
//!
//! Invariant (checked by tests after every block):
//! `native supply = balances + bonded stake + unbonding + reward escrow`.

use std::collections::BTreeMap;

use tenebra_tokenomics::{mul_div_ceil, mul_div_floor};

use crate::journal::{Journaled, JournaledCell};
use crate::tx::{Tx, TxKind};
use crate::types::*;

/// Fixed-point scale of per-share reward accumulators.
pub const ACC_PRECISION: u128 = 1_000_000_000_000_000_000;
const APP_HASH_CTX: &str = "Tenebra 2026-10-03 app hash v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Validator {
    pub operator: Address,
    pub consensus_key: [u8; 32],
    pub commission_bps: u32,
    /// Bonded stake (self + delegated). Slashing reduces this, which every
    /// delegator shares pro-rata through `shares`.
    pub tokens: Amount,
    pub shares: u128,
    pub acc_reward_per_share: u128,
    pub commission_owed: Amount,
    pub jailed: bool,
    pub jailed_until: i64,
    pub tombstoned: bool,
    pub missed_in_window: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delegation {
    pub shares: u128,
    pub acc_snapshot: u128,
    pub owed: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unbonding {
    pub delegator: Address,
    pub validator: Address,
    pub amount: Amount,
    pub creation_height: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Native coin supply.
    pub supply: Amount,
    /// Minted-but-unclaimed staking rewards and commissions.
    pub reward_escrow: Amount,
    pub total_issued: Amount,
    pub total_fees_burned: Amount,
    pub total_slashed: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteInfo {
    pub consensus_key: [u8; 32],
    pub power: u64,
    pub signed: bool,
}

/// Duplicate-vote evidence delivered by consensus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub consensus_key: [u8; 32],
    pub height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub height: u64,
    pub time: i64,
    /// Votes on the previous block (CometBFT `decided_last_commit`).
    pub last_votes: Vec<VoteInfo>,
    pub evidence: Vec<Evidence>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ValidatorUpdate {
    pub consensus_key: [u8; 32],
    /// 0 removes the validator from the consensus set.
    pub power: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SlashReason {
    DoubleSign,
    Downtime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockEvent {
    Issued {
        amount: Amount,
    },
    Slashed {
        validator: Address,
        amount: Amount,
        reason: SlashReason,
    },
    Jailed {
        validator: Address,
        until: i64,
    },
    UnbondingCompleted {
        delegator: Address,
        amount: Amount,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOutcome {
    pub result: Result<(), TxError>,
    pub gas_used: u64,
    pub fee_burned: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockResult {
    pub txs: Vec<TxOutcome>,
    pub events: Vec<BlockEvent>,
    pub validator_updates: Vec<ValidatorUpdate>,
    pub app_hash: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct GenesisValidator {
    pub operator: Address,
    pub consensus_key: [u8; 32],
    pub self_bond: Amount,
    pub commission_bps: u32,
}

#[derive(Clone, Debug)]
pub struct Genesis {
    pub params: ChainParams,
    pub genesis_time: i64,
    /// Non-native assets (e.g. bridged USDC) by id; id 0 is the native coin.
    pub assets: Vec<(AssetId, String)>,
    pub balances: Vec<(Address, AssetId, Amount)>,
    pub validators: Vec<GenesisValidator>,
}

#[derive(Clone, Debug)]
pub struct State {
    params: ChainParams,
    genesis_time: i64,
    height: u64,
    time: i64,
    last_issuance_time: i64,
    base_fee: Amount,
    block_gas_used: u64,
    assets: BTreeMap<AssetId, String>,
    active_set: BTreeMap<[u8; 32], u64>,
    counters: JournaledCell<Counters>,
    unbonding_seq: JournaledCell<u64>,
    balances: Journaled<(Address, AssetId), Amount>,
    nonces: Journaled<Address, u64>,
    validators: Journaled<Address, Validator>,
    consensus_keys: Journaled<[u8; 32], Address>,
    delegations: Journaled<(Address, Address), Delegation>,
    unbondings: Journaled<(i64, u64), Unbonding>,
}

fn add(a: Amount, b: Amount) -> Result<Amount, TxError> {
    a.checked_add(b).ok_or(TxError::Overflow)
}

fn sub(a: Amount, b: Amount) -> Result<Amount, TxError> {
    a.checked_sub(b).ok_or(TxError::Overflow)
}

fn bps_of(amount: Amount, bps: u32) -> Amount {
    // bps ≤ 10_000 (validated), so the product of the remainder cannot overflow.
    mul_div_floor(amount, bps as u128, BPS).unwrap_or(0)
}

fn valid_consensus_key(k: &[u8; 32]) -> bool {
    ed25519_dalek::VerifyingKey::from_bytes(k).is_ok_and(|v| !v.is_weak())
}

impl State {
    // ------------------------------------------------------------ genesis

    pub fn genesis(g: &Genesis) -> Result<(State, Vec<ValidatorUpdate>), &'static str> {
        g.params.validate()?;
        let mut assets = BTreeMap::new();
        assets.insert(NATIVE, g.params.native_symbol.clone());
        for (id, sym) in &g.assets {
            if *id == NATIVE || assets.insert(*id, sym.clone()).is_some() {
                return Err("duplicate or reserved asset id");
            }
        }
        let mut s = State {
            params: g.params.clone(),
            genesis_time: g.genesis_time,
            height: 0,
            time: g.genesis_time,
            last_issuance_time: g.genesis_time,
            base_fee: g.params.initial_base_fee,
            block_gas_used: 0,
            assets,
            active_set: BTreeMap::new(),
            counters: JournaledCell::new(Counters::default()),
            unbonding_seq: JournaledCell::new(0),
            balances: Journaled::default(),
            nonces: Journaled::default(),
            validators: Journaled::default(),
            consensus_keys: Journaled::default(),
            delegations: Journaled::default(),
            unbondings: Journaled::default(),
        };
        let mut c = Counters::default();
        for (addr, asset, amount) in &g.balances {
            if !s.assets.contains_key(asset) {
                return Err("genesis balance in unknown asset");
            }
            s.credit(*addr, *asset, *amount)
                .map_err(|_| "genesis overflow")?;
            if *asset == NATIVE {
                c.supply = add(c.supply, *amount).map_err(|_| "genesis overflow")?;
            }
        }
        for v in &g.validators {
            if !valid_consensus_key(&v.consensus_key) {
                return Err("invalid genesis consensus key");
            }
            if s.validators.contains_key(&v.operator)
                || s.consensus_keys.contains_key(&v.consensus_key)
            {
                return Err("duplicate genesis validator");
            }
            if v.self_bond < s.params.min_self_bond
                || v.commission_bps > s.params.max_commission_bps
            {
                return Err("genesis validator below min self-bond or above max commission");
            }
            s.validators.insert(
                v.operator,
                s.new_validator(v.operator, v.consensus_key, v.commission_bps, v.self_bond),
            );
            s.consensus_keys.insert(v.consensus_key, v.operator);
            s.delegations.insert(
                (v.operator, v.operator),
                Delegation {
                    shares: v.self_bond,
                    ..Default::default()
                },
            );
            c.supply = add(c.supply, v.self_bond).map_err(|_| "genesis overflow")?;
        }
        s.counters = JournaledCell::new(c);
        let updates = s.update_validator_set();
        if s.active_set.is_empty() {
            return Err("genesis has no eligible validator");
        }
        Ok((s, updates))
    }

    fn new_validator(
        &self,
        operator: Address,
        consensus_key: [u8; 32],
        commission_bps: u32,
        bond: Amount,
    ) -> Validator {
        Validator {
            operator,
            consensus_key,
            commission_bps,
            tokens: bond,
            shares: bond,
            acc_reward_per_share: 0,
            commission_owed: 0,
            jailed: false,
            jailed_until: 0,
            tombstoned: false,
            missed_in_window: 0,
        }
    }

    // ------------------------------------------------------------ queries

    pub fn params(&self) -> &ChainParams {
        &self.params
    }
    pub fn height(&self) -> u64 {
        self.height
    }
    pub fn time(&self) -> i64 {
        self.time
    }
    pub fn base_fee(&self) -> Amount {
        self.base_fee
    }
    pub fn counters(&self) -> &Counters {
        self.counters.get()
    }
    pub fn active_set(&self) -> &BTreeMap<[u8; 32], u64> {
        &self.active_set
    }
    pub fn balance(&self, a: Address, asset: AssetId) -> Amount {
        self.balances.get(&(a, asset)).copied().unwrap_or(0)
    }
    pub fn nonce(&self, a: Address) -> u64 {
        self.nonces.get(&a).copied().unwrap_or(0)
    }
    pub fn validator(&self, operator: Address) -> Option<&Validator> {
        self.validators.get(&operator)
    }
    pub fn delegation(&self, delegator: Address, validator: Address) -> Option<&Delegation> {
        self.delegations.get(&(delegator, validator))
    }
    pub fn unbondings(&self) -> impl Iterator<Item = &Unbonding> {
        self.unbondings.values()
    }

    /// Current value of a delegation in native base units.
    pub fn delegation_value(&self, delegator: Address, validator: Address) -> Amount {
        match (
            self.delegations.get(&(delegator, validator)),
            self.validators.get(&validator),
        ) {
            (Some(d), Some(v)) if v.shares > 0 => {
                mul_div_floor(d.shares, v.tokens, v.shares).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Unclaimed rewards a delegator could claim now (excluding commission).
    pub fn pending_rewards(&self, delegator: Address, validator: Address) -> Amount {
        match (
            self.delegations.get(&(delegator, validator)),
            self.validators.get(&validator),
        ) {
            (Some(d), Some(v)) => {
                let mut d = d.clone();
                let _ = settle(v, &mut d);
                d.owed
            }
            _ => 0,
        }
    }

    /// Σ of every place native coins can be. Equals `counters().supply`.
    pub fn native_holdings(&self) -> Amount {
        let bal: Amount = self
            .balances
            .iter()
            .filter(|((_, a), _)| *a == NATIVE)
            .map(|(_, v)| *v)
            .sum();
        let bonded: Amount = self.validators.values().map(|v| v.tokens).sum();
        let unbonding: Amount = self.unbondings.values().map(|u| u.amount).sum();
        bal + bonded + unbonding + self.counters.get().reward_escrow
    }

    /// Σ of all claimable rewards and commissions. Never exceeds the escrow.
    pub fn total_claimable(&self) -> Amount {
        let commissions: Amount = self.validators.values().map(|v| v.commission_owed).sum();
        let owed: Amount = self
            .delegations
            .iter()
            .map(|((_, val), d)| {
                let mut d = d.clone();
                if let Some(v) = self.validators.get(val) {
                    let _ = settle(v, &mut d);
                }
                d.owed
            })
            .sum();
        commissions + owed
    }

    // ------------------------------------------------------------ ledger

    fn credit(&mut self, a: Address, asset: AssetId, amount: Amount) -> Result<(), TxError> {
        if amount == 0 {
            return Ok(());
        }
        let b = add(self.balance(a, asset), amount)?;
        self.balances.insert((a, asset), b);
        Ok(())
    }

    fn debit(&mut self, a: Address, asset: AssetId, amount: Amount) -> Result<(), TxError> {
        let b = self.balance(a, asset);
        if b < amount {
            return Err(TxError::InsufficientBalance);
        }
        // Zero balances are removed so the state (and app hash) is canonical.
        if b == amount {
            self.balances.remove(&(a, asset));
        } else {
            self.balances.insert((a, asset), b - amount);
        }
        Ok(())
    }

    fn update_counters(
        &mut self,
        f: impl FnOnce(&mut Counters) -> Result<(), TxError>,
    ) -> Result<(), TxError> {
        let mut c = self.counters.get().clone();
        f(&mut c)?;
        self.counters.set(c);
        Ok(())
    }

    // ------------------------------------------------------------ transactions

    fn begin_tx(&mut self) {
        self.counters.begin();
        self.unbonding_seq.begin();
        self.balances.begin();
        self.nonces.begin();
        self.validators.begin();
        self.consensus_keys.begin();
        self.delegations.begin();
        self.unbondings.begin();
    }

    fn commit_tx(&mut self) {
        self.counters.commit();
        self.unbonding_seq.commit();
        self.balances.commit();
        self.nonces.commit();
        self.validators.commit();
        self.consensus_keys.commit();
        self.delegations.commit();
        self.unbondings.commit();
    }

    fn revert_tx(&mut self) {
        self.counters.revert();
        self.unbonding_seq.revert();
        self.balances.revert();
        self.nonces.revert();
        self.validators.revert();
        self.consensus_keys.revert();
        self.delegations.revert();
        self.unbondings.revert();
    }

    fn fee_for(&self, tx: &Tx) -> Result<(u64, Amount), TxError> {
        if tx.max_fee_per_gas < self.base_fee {
            return Err(TxError::FeeCapBelowBaseFee);
        }
        let gas = tx.kind.gas();
        let fee = (gas as u128)
            .checked_mul(self.base_fee)
            .ok_or(TxError::Overflow)?;
        Ok((gas, fee))
    }

    /// Mempool admission (CometBFT `CheckTx`): signature, chain, nonce not in
    /// the past, fee cap and ability to pay at the current base fee.
    pub fn check_tx(&self, tx: &Tx) -> Result<(), TxError> {
        if tx.chain_id != self.params.chain_id {
            return Err(TxError::WrongChain);
        }
        tx.verify()?;
        let expected = self.nonce(tx.sender);
        if tx.nonce < expected {
            return Err(TxError::BadNonce {
                expected,
                got: tx.nonce,
            });
        }
        let (_, fee) = self.fee_for(tx)?;
        if self.balance(tx.sender, NATIVE) < fee {
            return Err(TxError::CannotPayFee);
        }
        Ok(())
    }

    /// Execute one transaction inside the current block. Invalid
    /// transactions are rejected untouched; valid ones always pay (and burn)
    /// their gas fee, and their effects apply atomically or not at all.
    pub fn deliver_tx(&mut self, tx: &Tx) -> TxOutcome {
        let rejected = |e| TxOutcome {
            result: Err(e),
            gas_used: 0,
            fee_burned: 0,
        };
        if tx.chain_id != self.params.chain_id {
            return rejected(TxError::WrongChain);
        }
        if let Err(e) = tx.verify() {
            return rejected(e);
        }
        let expected = self.nonce(tx.sender);
        if tx.nonce != expected {
            return rejected(TxError::BadNonce {
                expected,
                got: tx.nonce,
            });
        }
        let (gas, fee) = match self.fee_for(tx) {
            Ok(x) => x,
            Err(e) => return rejected(e),
        };
        if self.block_gas_used.saturating_add(gas) > self.params.block_gas_limit {
            return rejected(TxError::BlockGasExceeded);
        }
        if self.balance(tx.sender, NATIVE) < fee {
            return rejected(TxError::CannotPayFee);
        }

        // Charge and burn the fee, bump the nonce: permanent even if execution fails.
        let charged = self.debit(tx.sender, NATIVE, fee).and_then(|_| {
            self.update_counters(|c| {
                c.supply = sub(c.supply, fee)?;
                c.total_fees_burned = add(c.total_fees_burned, fee)?;
                Ok(())
            })
        });
        if let Err(e) = charged {
            return rejected(e);
        }
        self.nonces.insert(tx.sender, expected + 1);
        self.block_gas_used += gas;

        self.begin_tx();
        let result = self.execute(tx.sender, &tx.kind);
        if result.is_ok() {
            self.commit_tx();
        } else {
            self.revert_tx();
        }
        TxOutcome {
            result,
            gas_used: gas,
            fee_burned: fee,
        }
    }

    fn execute(&mut self, sender: Address, kind: &TxKind) -> Result<(), TxError> {
        match *kind {
            TxKind::Transfer { to, asset, amount } => {
                if amount == 0 {
                    return Err(TxError::ZeroAmount);
                }
                if !self.assets.contains_key(&asset) {
                    return Err(TxError::UnknownAsset);
                }
                self.debit(sender, asset, amount)?;
                self.credit(to, asset, amount)
            }
            TxKind::RegisterValidator {
                consensus_key,
                commission_bps,
                self_bond,
            } => self.register_validator(sender, consensus_key, commission_bps, self_bond),
            TxKind::Delegate { validator, amount } => self.delegate(sender, validator, amount),
            TxKind::Undelegate { validator, amount } => self.undelegate(sender, validator, amount),
            TxKind::ClaimRewards { validator } => self.claim_rewards(sender, validator),
            TxKind::Unjail => self.unjail(sender),
        }
    }

    // ------------------------------------------------------------ staking txs

    fn register_validator(
        &mut self,
        sender: Address,
        key: [u8; 32],
        commission_bps: u32,
        self_bond: Amount,
    ) -> Result<(), TxError> {
        if self.validators.contains_key(&sender) {
            return Err(TxError::ValidatorExists);
        }
        if !valid_consensus_key(&key) {
            return Err(TxError::InvalidConsensusKey);
        }
        if self.consensus_keys.contains_key(&key) {
            return Err(TxError::ConsensusKeyInUse);
        }
        if commission_bps > self.params.max_commission_bps {
            return Err(TxError::CommissionTooHigh);
        }
        if self_bond < self.params.min_self_bond {
            return Err(TxError::SelfBondTooLow);
        }
        self.debit(sender, NATIVE, self_bond)?;
        self.validators.insert(
            sender,
            self.new_validator(sender, key, commission_bps, self_bond),
        );
        self.consensus_keys.insert(key, sender);
        self.delegations.insert(
            (sender, sender),
            Delegation {
                shares: self_bond,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn delegate(
        &mut self,
        sender: Address,
        validator: Address,
        amount: Amount,
    ) -> Result<(), TxError> {
        if amount == 0 {
            return Err(TxError::ZeroAmount);
        }
        let mut v = self
            .validators
            .get(&validator)
            .cloned()
            .ok_or(TxError::UnknownValidator)?;
        if v.tombstoned {
            return Err(TxError::Tombstoned);
        }
        if v.tokens == 0 && v.shares > 0 {
            return Err(TxError::ValidatorFullySlashed);
        }
        let new_shares = if v.shares == 0 {
            amount
        } else {
            // Round down: the newcomer never dilutes existing delegators.
            mul_div_floor(amount, v.shares, v.tokens).ok_or(TxError::Overflow)?
        };
        if new_shares == 0 {
            return Err(TxError::ZeroAmount);
        }
        self.debit(sender, NATIVE, amount)?;
        let mut d = self
            .delegations
            .get(&(sender, validator))
            .cloned()
            .unwrap_or_default();
        settle(&v, &mut d)?;
        d.shares = add(d.shares, new_shares)?;
        v.tokens = add(v.tokens, amount)?;
        v.shares = add(v.shares, new_shares)?;
        self.delegations.insert((sender, validator), d);
        self.validators.insert(validator, v);
        Ok(())
    }

    fn undelegate(
        &mut self,
        sender: Address,
        validator: Address,
        amount: Amount,
    ) -> Result<(), TxError> {
        if amount == 0 {
            return Err(TxError::ZeroAmount);
        }
        let mut v = self
            .validators
            .get(&validator)
            .cloned()
            .ok_or(TxError::UnknownValidator)?;
        let mut d = self
            .delegations
            .get(&(sender, validator))
            .cloned()
            .ok_or(TxError::InsufficientDelegation)?;
        if v.tokens == 0 {
            return Err(TxError::ValidatorFullySlashed);
        }
        settle(&v, &mut d)?;
        // Round shares up: the leaver never takes value from those who stay.
        let shares = mul_div_ceil(amount, v.shares, v.tokens).ok_or(TxError::Overflow)?;
        if shares > d.shares || amount > v.tokens {
            return Err(TxError::InsufficientDelegation);
        }
        // The last shares out take any rounding remainder with them.
        let paid = if shares == v.shares { v.tokens } else { amount };
        v.tokens -= paid;
        v.shares -= shares;
        d.shares -= shares;

        let seq = *self.unbonding_seq.get();
        self.unbonding_seq.set(seq + 1);
        let completion = self.time.saturating_add(self.params.unbonding_secs);
        self.unbondings.insert(
            (completion, seq),
            Unbonding {
                delegator: sender,
                validator,
                amount: paid,
                creation_height: self.height,
            },
        );
        if d.shares == 0 && d.owed == 0 {
            self.delegations.remove(&(sender, validator));
        } else {
            self.delegations.insert((sender, validator), d);
        }
        self.validators.insert(validator, v);
        Ok(())
    }

    fn claim_rewards(&mut self, sender: Address, validator: Address) -> Result<(), TxError> {
        let mut v = self
            .validators
            .get(&validator)
            .cloned()
            .ok_or(TxError::UnknownValidator)?;
        let mut total = 0;
        if let Some(mut d) = self.delegations.get(&(sender, validator)).cloned() {
            settle(&v, &mut d)?;
            total = d.owed;
            d.owed = 0;
            if d.shares == 0 {
                self.delegations.remove(&(sender, validator));
            } else {
                self.delegations.insert((sender, validator), d);
            }
        }
        if v.operator == sender && v.commission_owed > 0 {
            total = add(total, v.commission_owed)?;
            v.commission_owed = 0;
            self.validators.insert(validator, v);
        }
        if total == 0 {
            return Err(TxError::NothingToClaim);
        }
        self.update_counters(|c| {
            c.reward_escrow = sub(c.reward_escrow, total)?;
            Ok(())
        })?;
        self.credit(sender, NATIVE, total)
    }

    fn unjail(&mut self, sender: Address) -> Result<(), TxError> {
        let mut v = self
            .validators
            .get(&sender)
            .cloned()
            .ok_or(TxError::UnknownValidator)?;
        if v.tombstoned {
            return Err(TxError::Tombstoned);
        }
        if !v.jailed {
            return Err(TxError::NotJailed);
        }
        if self.time < v.jailed_until {
            return Err(TxError::StillJailed);
        }
        v.jailed = false;
        v.missed_in_window = 0;
        self.validators.insert(sender, v);
        Ok(())
    }

    // ------------------------------------------------------------ block lifecycle

    pub fn issuance_rate_bps(&self, now: i64) -> u32 {
        let elapsed = now.saturating_sub(self.genesis_time).max(0) as u128;
        let decay = mul_div_floor(
            self.params.issuance_decay_bps_per_year as u128,
            elapsed,
            SECONDS_PER_YEAR,
        )
        .unwrap_or(u128::MAX);
        let rate = (self.params.initial_issuance_bps as u128).saturating_sub(decay);
        // ≤ initial_issuance_bps, which is a u32.
        u32::try_from(rate)
            .unwrap_or(0)
            .max(self.params.issuance_floor_bps)
    }

    pub fn begin_block(&mut self, h: &BlockHeader) -> Result<Vec<BlockEvent>, &'static str> {
        if h.height != self.height + 1 {
            return Err("non-sequential height");
        }
        if h.time < self.time {
            return Err("time went backwards");
        }
        self.height = h.height;
        self.time = h.time;
        self.block_gas_used = 0;
        let mut events = Vec::new();

        for e in &h.evidence {
            self.punish_double_sign(e, &mut events);
        }
        self.track_downtime(&h.last_votes, &mut events);
        self.mint_issuance(&h.last_votes, &mut events);
        Ok(events)
    }

    fn punish_double_sign(&mut self, e: &Evidence, events: &mut Vec<BlockEvent>) {
        let Some(op) = self.consensus_keys.get(&e.consensus_key).copied() else {
            return;
        };
        let Some(mut v) = self.validators.get(&op).cloned() else {
            return;
        };
        if v.tombstoned {
            return; // one punishment per validator lifetime
        }
        let amount = self.slash(op, self.params.slash_double_sign_bps, e.height);
        v = self.validators.get(&op).cloned().unwrap_or(v);
        v.tombstoned = true;
        v.jailed = true;
        v.jailed_until = i64::MAX;
        self.validators.insert(op, v);
        events.push(BlockEvent::Slashed {
            validator: op,
            amount,
            reason: SlashReason::DoubleSign,
        });
        events.push(BlockEvent::Jailed {
            validator: op,
            until: i64::MAX,
        });
    }

    /// Burn `bps` of the validator's bonded stake and of unbondings that
    /// started at or after the infraction (they were still at stake then).
    fn slash(&mut self, op: Address, bps: u32, infraction_height: u64) -> Amount {
        let Some(mut v) = self.validators.get(&op).cloned() else {
            return 0;
        };
        let mut total = bps_of(v.tokens, bps);
        v.tokens -= total;
        self.validators.insert(op, v);

        let keys: Vec<(i64, u64)> = self
            .unbondings
            .iter()
            .filter(|(_, u)| u.validator == op && u.creation_height >= infraction_height)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(mut u) = self.unbondings.get(&k).cloned() {
                let cut = bps_of(u.amount, bps);
                u.amount -= cut;
                total += cut;
                self.unbondings.insert(k, u);
            }
        }
        let mut c = self.counters.get().clone();
        c.supply -= total;
        c.total_slashed += total;
        self.counters.set(c);
        total
    }

    fn track_downtime(&mut self, votes: &[VoteInfo], events: &mut Vec<BlockEvent>) {
        if self.height % self.params.downtime_window_blocks == 0 {
            let ops: Vec<Address> = self
                .validators
                .iter()
                .filter(|(_, v)| v.missed_in_window > 0)
                .map(|(k, _)| *k)
                .collect();
            for op in ops {
                if let Some(mut v) = self.validators.get(&op).cloned() {
                    v.missed_in_window = 0;
                    self.validators.insert(op, v);
                }
            }
        }
        for vote in votes.iter().filter(|v| !v.signed) {
            let Some(op) = self.consensus_keys.get(&vote.consensus_key).copied() else {
                continue;
            };
            let Some(mut v) = self.validators.get(&op).cloned() else {
                continue;
            };
            if v.jailed {
                continue;
            }
            v.missed_in_window += 1;
            let exceeded = v.missed_in_window > self.params.max_missed_in_window;
            self.validators.insert(op, v);
            if exceeded {
                let amount = self.slash(op, self.params.slash_downtime_bps, self.height);
                if let Some(mut v) = self.validators.get(&op).cloned() {
                    v.jailed = true;
                    v.jailed_until = self.time.saturating_add(self.params.downtime_jail_secs);
                    v.missed_in_window = 0;
                    let until = v.jailed_until;
                    self.validators.insert(op, v);
                    events.push(BlockEvent::Slashed {
                        validator: op,
                        amount,
                        reason: SlashReason::Downtime,
                    });
                    events.push(BlockEvent::Jailed {
                        validator: op,
                        until,
                    });
                }
            }
        }
    }

    /// Mint this interval's issuance and split it across validators that
    /// signed the previous block, by voting power. Commission goes to the
    /// operator; the rest accrues per share to all delegators.
    fn mint_issuance(&mut self, votes: &[VoteInfo], events: &mut Vec<BlockEvent>) {
        let dt = (self.time - self.last_issuance_time)
            .clamp(0, self.params.max_issuance_interval_secs) as u128;
        self.last_issuance_time = self.time;
        let rate = self.issuance_rate_bps(self.time) as u128;
        let supply = self.counters.get().supply;
        let Some(budget) = rate
            .checked_mul(dt)
            .and_then(|x| mul_div_floor(supply, x, BPS * SECONDS_PER_YEAR))
        else {
            return;
        };

        let signers: Vec<(Address, u64)> = votes
            .iter()
            .filter(|v| v.signed && v.power > 0)
            .filter_map(|v| {
                self.consensus_keys
                    .get(&v.consensus_key)
                    .map(|op| (*op, v.power))
            })
            .filter(|(op, _)| self.validators.get(op).is_some_and(|v| !v.jailed))
            .collect();
        let total_power: u128 = signers.iter().map(|(_, p)| *p as u128).sum();
        if budget == 0 || total_power == 0 {
            return;
        }

        let mut minted = 0u128;
        for (op, power) in signers {
            let reward = mul_div_floor(budget, power as u128, total_power).unwrap_or(0);
            let Some(mut v) = self.validators.get(&op).cloned() else {
                continue;
            };
            let commission = if v.shares == 0 {
                reward
            } else {
                bps_of(reward, v.commission_bps)
            };
            let rest = reward - commission;
            v.commission_owed += commission;
            if rest > 0 {
                // Rounding dust stays in escrow, so escrow ≥ Σ claimable.
                v.acc_reward_per_share += mul_div_floor(rest, ACC_PRECISION, v.shares).unwrap_or(0);
            }
            self.validators.insert(op, v);
            minted += reward;
        }
        let mut c = self.counters.get().clone();
        c.supply += minted;
        c.reward_escrow += minted;
        c.total_issued += minted;
        self.counters.set(c);
        events.push(BlockEvent::Issued { amount: minted });
    }

    pub fn end_block(&mut self) -> (Vec<ValidatorUpdate>, Vec<BlockEvent>) {
        let mut events = Vec::new();
        let due: Vec<(i64, u64)> = self
            .unbondings
            .range(..=(self.time, u64::MAX))
            .map(|(k, _)| *k)
            .collect();
        for k in due {
            if let Some(u) = self.unbondings.remove(&k) {
                // Credits cannot overflow: the amount came out of the same supply.
                let _ = self.credit(u.delegator, NATIVE, u.amount);
                events.push(BlockEvent::UnbondingCompleted {
                    delegator: u.delegator,
                    amount: u.amount,
                });
            }
        }
        self.update_base_fee();
        (self.update_validator_set(), events)
    }

    /// EIP-1559: move toward the gas target by at most 1/denominator per block.
    fn update_base_fee(&mut self) {
        let (used, target) = (
            self.block_gas_used as u128,
            self.params.block_gas_target as u128,
        );
        let d = self.params.base_fee_change_denominator;
        let base = self.base_fee;
        self.base_fee = if used > target {
            let delta = (mul_div_floor(base, used - target, target).unwrap_or(base) / d).max(1);
            base.saturating_add(delta)
        } else {
            let delta = mul_div_floor(base, target - used, target).unwrap_or(0) / d;
            base.saturating_sub(delta).max(self.params.min_base_fee)
        };
    }

    fn eligible(&self, v: &Validator) -> bool {
        !v.jailed
            && !v.tombstoned
            && v.tokens >= self.params.power_reduction
            && self.delegation_value(v.operator, v.operator) >= self.params.min_self_bond
    }

    /// Top `max_validators` eligible validators by stake (ties by consensus
    /// key). Returns only the changes, as CometBFT expects.
    fn update_validator_set(&mut self) -> Vec<ValidatorUpdate> {
        let mut candidates: Vec<&Validator> = self
            .validators
            .values()
            .filter(|v| self.eligible(v))
            .collect();
        candidates.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then(a.consensus_key.cmp(&b.consensus_key))
        });
        // Keep total power within CometBFT's i64::MAX / 8 bound.
        let max_power = (i64::MAX as u128 / 8) / self.params.max_validators as u128;
        let new: BTreeMap<[u8; 32], u64> = candidates
            .into_iter()
            .take(self.params.max_validators as usize)
            .map(|v| {
                let p = (v.tokens / self.params.power_reduction).min(max_power);
                (v.consensus_key, u64::try_from(p).unwrap_or(u64::MAX))
            })
            .collect();

        let mut updates = Vec::new();
        for k in self.active_set.keys() {
            if !new.contains_key(k) {
                updates.push(ValidatorUpdate {
                    consensus_key: *k,
                    power: 0,
                });
            }
        }
        for (k, p) in &new {
            if self.active_set.get(k) != Some(p) {
                updates.push(ValidatorUpdate {
                    consensus_key: *k,
                    power: *p,
                });
            }
        }
        updates.sort_by_key(|u| u.consensus_key);
        self.active_set = new;
        updates
    }

    /// Deterministic hash of the full state (CometBFT `app_hash`).
    pub fn app_hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key(APP_HASH_CTX);
        let c = self.counters.get();
        h.update(&self.params.chain_id.to_le_bytes());
        h.update(&self.height.to_le_bytes());
        h.update(&self.time.to_le_bytes());
        h.update(&self.last_issuance_time.to_le_bytes());
        h.update(&self.base_fee.to_le_bytes());
        for x in [
            c.supply,
            c.reward_escrow,
            c.total_issued,
            c.total_fees_burned,
            c.total_slashed,
        ] {
            h.update(&x.to_le_bytes());
        }
        h.update(&self.unbonding_seq.get().to_le_bytes());
        for ((a, asset), v) in self.balances.iter() {
            h.update(b"B")
                .update(&a.0)
                .update(&asset.to_le_bytes())
                .update(&v.to_le_bytes());
        }
        for (a, n) in self.nonces.iter() {
            h.update(b"N").update(&a.0).update(&n.to_le_bytes());
        }
        for (op, v) in self.validators.iter() {
            h.update(b"V")
                .update(&op.0)
                .update(&v.consensus_key)
                .update(&v.commission_bps.to_le_bytes());
            for x in [
                v.tokens,
                v.shares,
                v.acc_reward_per_share,
                v.commission_owed,
            ] {
                h.update(&x.to_le_bytes());
            }
            h.update(&[v.jailed as u8, v.tombstoned as u8])
                .update(&v.jailed_until.to_le_bytes());
            h.update(&v.missed_in_window.to_le_bytes());
        }
        for ((d, val), x) in self.delegations.iter() {
            h.update(b"D").update(&d.0).update(&val.0);
            for y in [x.shares, x.acc_snapshot, x.owed] {
                h.update(&y.to_le_bytes());
            }
        }
        for ((t, seq), u) in self.unbondings.iter() {
            h.update(b"U")
                .update(&t.to_le_bytes())
                .update(&seq.to_le_bytes());
            h.update(&u.delegator.0)
                .update(&u.validator.0)
                .update(&u.amount.to_le_bytes())
                .update(&u.creation_height.to_le_bytes());
        }
        for (k, p) in &self.active_set {
            h.update(b"A").update(k).update(&p.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }

    /// ABCI `FinalizeBlock` + `Commit` in one call.
    pub fn finalize_block(
        &mut self,
        header: &BlockHeader,
        txs: &[Tx],
    ) -> Result<BlockResult, &'static str> {
        let mut events = self.begin_block(header)?;
        let outcomes = txs.iter().map(|t| self.deliver_tx(t)).collect();
        let (validator_updates, more) = self.end_block();
        events.extend(more);
        Ok(BlockResult {
            txs: outcomes,
            events,
            validator_updates,
            app_hash: self.app_hash(),
        })
    }
}

/// Credit everything `d` earned since its last snapshot (only rounds down).
fn settle(v: &Validator, d: &mut Delegation) -> Result<(), TxError> {
    let delta = v.acc_reward_per_share.saturating_sub(d.acc_snapshot);
    let gain = mul_div_floor(delta, d.shares, ACC_PRECISION).ok_or(TxError::Overflow)?;
    d.owed = add(d.owed, gain)?;
    d.acc_snapshot = v.acc_reward_per_share;
    Ok(())
}
