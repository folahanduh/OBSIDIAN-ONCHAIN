use anchor_lang::prelude::*;
use tenebra_tokenomics as tk;

pub const MAX_ROUTERS: usize = 4;
pub const MAX_FEE_MINTS: usize = 2;
pub const DENYLIST_CAPACITY: usize = 256;

pub const CONFIG_SEED: &[u8] = b"config";
pub const STAKING_SEED: &[u8] = b"staking";
pub const DENYLIST_SEED: &[u8] = b"denylist";
pub const POSITION_SEED: &[u8] = b"position";
pub const SESSION_SEED: &[u8] = b"session";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault_authority";
pub const STAKE_VAULT_SEED: &[u8] = b"stake_vault";
pub const FEE_VAULT_SEED: &[u8] = b"fee_vault";
pub const REWARD_VAULT_SEED: &[u8] = b"reward_vault";
pub const BUYBACK_VAULT_SEED: &[u8] = b"buyback_vault";

/// Tunable economics. Mirrors `tenebra_tokenomics` types field-for-field so
/// the account layout is explicit and stable.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Economics {
    pub markup_min_ppm: u32,
    pub markup_max_ppm: u32,
    pub discount_cap_ppm: u32,
    pub stake_for_cap: u64,
    pub tier1: u64,
    pub tier2: u64,
    pub tier3: u64,
    pub zero_fee_tier: u8,
    pub warmup_secs: i64,
    pub cooldown_secs: i64,
    pub treasury_bps: u16,
    pub burn_base_bps: u16,
    pub burn_min_bps: u16,
    pub burn_max_bps: u16,
    pub burn_slope_bps: u16,
    /// Weight of the newest epoch in the revenue moving average.
    pub revenue_alpha_ppm: u32,
    /// Minimum seconds between `distribute` calls per fee pool.
    pub epoch_secs: i64,
}

impl Economics {
    pub fn fee_policy(&self) -> tk::FeePolicy {
        tk::FeePolicy {
            markup: tk::MarkupBounds {
                min_ppm: self.markup_min_ppm,
                max_ppm: self.markup_max_ppm,
            },
            discount: tk::DiscountCurve {
                cap_ppm: self.discount_cap_ppm,
                stake_for_cap: self.stake_for_cap,
            },
            tiers: tk::TierThresholds {
                tier1: self.tier1,
                tier2: self.tier2,
                tier3: self.tier3,
            },
            zero_fee_tier: self.zero_fee_tier,
        }
    }

    pub fn timing(&self) -> tk::StakeTiming {
        tk::StakeTiming {
            warmup_secs: self.warmup_secs,
            cooldown_secs: self.cooldown_secs,
        }
    }

    pub fn split(&self) -> tk::SplitParams {
        tk::SplitParams {
            treasury_bps: self.treasury_bps,
            burn: tk::BurnCurve {
                base_bps: self.burn_base_bps,
                min_bps: self.burn_min_bps,
                max_bps: self.burn_max_bps,
                slope_bps: self.burn_slope_bps,
            },
        }
    }

    pub fn validate(&self) -> core::result::Result<(), tk::TokenomicsError> {
        self.fee_policy().validate()?;
        self.timing().validate()?;
        self.split().validate()?;
        if self.epoch_secs <= 0 || self.revenue_alpha_ppm as u64 > tk::PPM {
            return Err(tk::TokenomicsError::InvalidConfig);
        }
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Maintains the sanctions denylist; separate key from `admin`.
    pub compliance_authority: Pubkey,
    /// Owner of the treasury's token accounts.
    pub treasury: Pubkey,
    pub stake_mint: Pubkey,
    pub paused: bool,
    /// Programs a guarded swap may route through (e.g. Jupiter V6).
    pub routers: [Pubkey; MAX_ROUTERS],
    pub router_count: u8,
    /// Mints fees are collected in (e.g. USDC, wSOL). Fixed at initialize.
    pub fee_mints: [Pubkey; MAX_FEE_MINTS],
    pub fee_mint_count: u8,
    pub economics: Economics,
    pub bump: u8,
    pub vault_authority_bump: u8,
}

impl Config {
    pub fn routers(&self) -> &[Pubkey] {
        &self.routers[..self.router_count as usize]
    }

    pub fn fee_pool_index(&self, mint: &Pubkey) -> Option<usize> {
        self.fee_mints[..self.fee_mint_count as usize]
            .iter()
            .position(|m| m == mint)
    }
}

#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace,
)]
pub struct PoolState {
    pub mint: Pubkey,
    pub initialized: bool,
    pub acc_per_share: u128,
    pub undistributed: u64,
    pub ewma_scaled: u128,
    pub epochs: u64,
    pub last_distribution_ts: i64,
    // Lifetime totals, for transparency dashboards.
    pub total_fees: u64,
    pub total_treasury: u64,
    pub total_buyback: u64,
    pub total_rewards: u64,
}

impl PoolState {
    pub fn reward_pool(&self) -> tk::RewardPool {
        tk::RewardPool {
            acc_per_share: self.acc_per_share,
            undistributed: self.undistributed,
        }
    }

    pub fn store_reward_pool(&mut self, p: &tk::RewardPool) {
        self.acc_per_share = p.acc_per_share;
        self.undistributed = p.undistributed;
    }

    pub fn tracker(&self) -> tk::RevenueTracker {
        tk::RevenueTracker {
            ewma_scaled: self.ewma_scaled,
            epochs: self.epochs,
        }
    }

    pub fn store_tracker(&mut self, t: &tk::RevenueTracker) {
        self.ewma_scaled = t.ewma_scaled;
        self.epochs = t.epochs;
    }
}

#[account]
#[derive(InitSpace)]
pub struct StakingState {
    pub total_staked: u64,
    pub pools: [PoolState; MAX_FEE_MINTS],
    pub bump: u8,
}

#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace,
)]
pub struct Checkpoint {
    pub acc_snapshot: u128,
    pub owed: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Position {
    pub owner: Pubkey,
    pub matured: u64,
    pub pending: u64,
    pub pending_since: i64,
    pub cooling: u64,
    pub cooldown_ends: i64,
    pub checkpoints: [Checkpoint; MAX_FEE_MINTS],
    pub bump: u8,
}

impl Position {
    pub fn stake(&self) -> tk::StakePosition {
        tk::StakePosition {
            matured: self.matured,
            pending: self.pending,
            pending_since: self.pending_since,
            cooling: self.cooling,
            cooldown_ends: self.cooldown_ends,
        }
    }

    pub fn store_stake(&mut self, s: &tk::StakePosition) {
        self.matured = s.matured;
        self.pending = s.pending;
        self.pending_since = s.pending_since;
        self.cooling = s.cooling;
        self.cooldown_ends = s.cooldown_ends;
    }

    pub fn checkpoint(&self, i: usize) -> tk::RewardCheckpoint {
        tk::RewardCheckpoint {
            acc_snapshot: self.checkpoints[i].acc_snapshot,
            owed: self.checkpoints[i].owed,
        }
    }

    pub fn store_checkpoint(&mut self, i: usize, cp: &tk::RewardCheckpoint) {
        self.checkpoints[i] = Checkpoint {
            acc_snapshot: cp.acc_snapshot,
            owed: cp.owed,
        };
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum FeeSide {
    Input,
    Output,
}

/// Lives only between `pre_swap` and `post_swap` in one transaction.
#[account]
#[derive(InitSpace)]
pub struct SwapSession {
    pub user: Pubkey,
    pub in_account: Pubkey,
    pub out_account: Pubkey,
    pub fee_mint: Pubkey,
    pub fee_side: FeeSide,
    /// Input balance after the input-side fee (if any) was taken.
    pub in_balance_start: u64,
    pub out_balance_start: u64,
    pub max_in: u64,
    pub min_out: u64,
    pub markup_ppm: u32,
    pub discount_ppm: u32,
    pub tier: u8,
    pub fee_waived: bool,
    pub input_fee_paid: u64,
    pub bump: u8,
}

/// Sorted list of sanctioned addresses, maintained by the compliance
/// authority. Binary-searched on every swap.
#[account]
#[derive(InitSpace)]
pub struct Denylist {
    #[max_len(DENYLIST_CAPACITY)]
    pub entries: Vec<Pubkey>,
    pub bump: u8,
}

impl Denylist {
    pub fn contains(&self, key: &Pubkey) -> bool {
        self.entries.binary_search(key).is_ok()
    }
}
