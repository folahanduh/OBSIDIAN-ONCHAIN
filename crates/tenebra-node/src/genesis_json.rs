//! `app_state` section of CometBFT's genesis.json. Amounts are decimal
//! strings (u128 does not fit in JSON numbers); keys are hex.

use serde::{Deserialize, Serialize};
use tenebra_chain::{Address, Amount, ChainParams, Genesis, GenesisValidator};

use crate::hex;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ParamsJson {
    pub chain_id: u64,
    pub native_symbol: String,
    pub native_decimals: u8,
    pub block_gas_limit: u64,
    pub block_gas_target: u64,
    pub initial_base_fee: String,
    pub min_base_fee: String,
    pub base_fee_change_denominator: String,
    pub max_validators: u32,
    pub min_self_bond: String,
    pub max_commission_bps: u32,
    pub unbonding_secs: i64,
    pub power_reduction: String,
    pub initial_issuance_bps: u32,
    pub issuance_floor_bps: u32,
    pub issuance_decay_bps_per_year: u32,
    pub max_issuance_interval_secs: i64,
    pub slash_double_sign_bps: u32,
    pub slash_downtime_bps: u32,
    pub downtime_window_blocks: u64,
    pub max_missed_in_window: u64,
    pub downtime_jail_secs: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssetJson {
    pub id: u32,
    pub symbol: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BalanceJson {
    pub address: String,
    pub asset: u32,
    pub amount: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ValidatorJson {
    pub operator: String,
    pub consensus_key: String,
    pub self_bond: String,
    pub commission_bps: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppGenesis {
    pub params: ParamsJson,
    #[serde(default)]
    pub assets: Vec<AssetJson>,
    #[serde(default)]
    pub balances: Vec<BalanceJson>,
    pub validators: Vec<ValidatorJson>,
}

fn amt(s: &str, what: &str) -> Result<Amount, String> {
    s.parse()
        .map_err(|_| format!("{what}: not a non-negative integer: {s:?}"))
}

fn addr(s: &str) -> Result<Address, String> {
    hex::decode32(s)
        .map(Address)
        .ok_or_else(|| format!("bad 32-byte hex: {s:?}"))
}

impl ParamsJson {
    /// Devnet defaults: 9 decimals, 1 coin = 1 unit of voting power.
    pub fn devnet(chain_id: u64, symbol: &str) -> ParamsJson {
        ParamsJson {
            chain_id,
            native_symbol: symbol.into(),
            native_decimals: 9,
            block_gas_limit: 10_000_000,
            block_gas_target: 5_000_000,
            initial_base_fee: "1000".into(),
            min_base_fee: "100".into(),
            base_fee_change_denominator: "8".into(),
            max_validators: 100,
            min_self_bond: "1000000000000".into(), // 1,000 coins
            max_commission_bps: 2_000,
            unbonding_secs: 21 * 86_400,
            power_reduction: "1000000000".into(),
            initial_issuance_bps: 800,
            issuance_floor_bps: 150,
            issuance_decay_bps_per_year: 100,
            max_issuance_interval_secs: 3_600,
            slash_double_sign_bps: 500,
            slash_downtime_bps: 1,
            downtime_window_blocks: 10_000,
            max_missed_in_window: 9_500,
            downtime_jail_secs: 600,
        }
    }

    fn to_params(&self) -> Result<ChainParams, String> {
        Ok(ChainParams {
            chain_id: self.chain_id,
            native_symbol: self.native_symbol.clone(),
            native_decimals: self.native_decimals,
            block_gas_limit: self.block_gas_limit,
            block_gas_target: self.block_gas_target,
            initial_base_fee: amt(&self.initial_base_fee, "initial_base_fee")?,
            min_base_fee: amt(&self.min_base_fee, "min_base_fee")?,
            base_fee_change_denominator: amt(
                &self.base_fee_change_denominator,
                "base_fee_change_denominator",
            )?,
            max_validators: self.max_validators,
            min_self_bond: amt(&self.min_self_bond, "min_self_bond")?,
            max_commission_bps: self.max_commission_bps,
            unbonding_secs: self.unbonding_secs,
            power_reduction: amt(&self.power_reduction, "power_reduction")?,
            initial_issuance_bps: self.initial_issuance_bps,
            issuance_floor_bps: self.issuance_floor_bps,
            issuance_decay_bps_per_year: self.issuance_decay_bps_per_year,
            max_issuance_interval_secs: self.max_issuance_interval_secs,
            slash_double_sign_bps: self.slash_double_sign_bps,
            slash_downtime_bps: self.slash_downtime_bps,
            downtime_window_blocks: self.downtime_window_blocks,
            max_missed_in_window: self.max_missed_in_window,
            downtime_jail_secs: self.downtime_jail_secs,
        })
    }
}

impl AppGenesis {
    pub fn parse(bytes: &[u8], genesis_time: i64) -> Result<Genesis, String> {
        let g: AppGenesis = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        g.to_genesis(genesis_time)
    }

    pub fn to_genesis(&self, genesis_time: i64) -> Result<Genesis, String> {
        Ok(Genesis {
            params: self.params.to_params()?,
            genesis_time,
            assets: self
                .assets
                .iter()
                .map(|a| (a.id, a.symbol.clone()))
                .collect(),
            balances: self
                .balances
                .iter()
                .map(|b| Ok((addr(&b.address)?, b.asset, amt(&b.amount, "balance")?)))
                .collect::<Result<_, String>>()?,
            validators: self
                .validators
                .iter()
                .map(|v| {
                    Ok(GenesisValidator {
                        operator: addr(&v.operator)?,
                        consensus_key: hex::decode32(&v.consensus_key)
                            .ok_or("bad consensus key hex")?,
                        self_bond: amt(&v.self_bond, "self_bond")?,
                        commission_bps: v.commission_bps,
                    })
                })
                .collect::<Result<_, String>>()?,
        })
    }
}
