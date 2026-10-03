//! Tenebra guard: an on-chain introspection guard for routed swaps, plus
//! staking and real-yield revenue distribution.
//!
//! All economics live in `tenebra_tokenomics` (pure, property-tested); this
//! program validates accounts, moves tokens and stores state.

#![allow(unexpected_cfgs)]
// Anchor 0.31's `#[program]` expansion calls the deprecated `AccountInfo::realloc`
// in generated IDL code; there is no narrower scope that reaches it. Remove when
// moving to an Anchor release whose CPIs work under solana-program-test.
#![allow(deprecated)]

use anchor_lang::prelude::*;

pub mod error;
pub mod instructions;
pub mod introspection;
pub mod state;

use instructions::*;
use state::FeeSide;

declare_id!("HueCcTwE36KfZhqbopNCN7ckisnPH24meaSmEK29h3KC");

#[program]
pub mod tenebra_guard {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, params: InitParams) -> Result<()> {
        instructions::admin::initialize(ctx, params)
    }

    pub fn init_fee_pool(ctx: Context<InitFeePool>) -> Result<()> {
        instructions::admin::init_fee_pool(ctx)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, params: UpdateParams) -> Result<()> {
        instructions::admin::update_config(ctx, params)
    }

    pub fn update_denylist(
        ctx: Context<UpdateDenylist>,
        add: Vec<Pubkey>,
        remove: Vec<Pubkey>,
    ) -> Result<()> {
        instructions::admin::update_denylist(ctx, add, remove)
    }

    pub fn pre_swap(ctx: Context<PreSwap>, args: PreSwapArgs) -> Result<()> {
        instructions::swap::pre_swap(ctx, args)
    }

    pub fn post_swap(ctx: Context<PostSwap>) -> Result<()> {
        instructions::swap::post_swap(ctx)
    }

    pub fn open_position(ctx: Context<OpenPosition>) -> Result<()> {
        instructions::staking::open_position(ctx)
    }

    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        instructions::staking::stake(ctx, amount)
    }

    pub fn request_unstake(ctx: Context<RequestUnstake>, amount: u64) -> Result<()> {
        instructions::staking::request_unstake(ctx, amount)
    }

    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        instructions::staking::withdraw(ctx)
    }

    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        instructions::staking::claim(ctx)
    }

    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        instructions::distribute::distribute(ctx)
    }
}

// Keep `FeeSide` reachable from the crate root for clients.
pub type SwapFeeSide = FeeSide;
