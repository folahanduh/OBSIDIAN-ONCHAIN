//! Permissionless per-epoch revenue split:
//! fee vault → treasury / buyback vault / staker reward vault.
//!
//! The buyback share accumulates in the buyback vault in the fee currency.
//! Executing the buyback (swap → burn) needs a PDA-signed router CPI with an
//! oracle-bounded minimum price; that is deliberately left for v2 rather than
//! handing a hot key discretionary access to the funds.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use tenebra_tokenomics as tk;

use crate::error::{GuardError, TkResultExt};
use crate::state::*;

#[derive(Accounts)]
pub struct Distribute<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Box<Account<'info, StakingState>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [FEE_VAULT_SEED, mint.key().as_ref()], bump)]
    pub fee_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [REWARD_VAULT_SEED, mint.key().as_ref()], bump)]
    pub reward_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [BUYBACK_VAULT_SEED, mint.key().as_ref()], bump)]
    pub buyback_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = mint, token::authority = config.treasury)]
    pub treasury_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: vault authority PDA.
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
}

#[event]
pub struct Distributed {
    pub mint: Pubkey,
    pub amount: u64,
    pub intensity_ppm: u64,
    pub burn_bps: u16,
    pub treasury: u64,
    pub buyback: u64,
    pub stakers: u64,
    pub total_staked: u64,
}

fn send<'info>(
    a: &Distribute<'info>,
    to: &InterfaceAccount<'info, TokenAccount>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let seeds: &[&[u8]] = &[VAULT_AUTHORITY_SEED, &[a.config.vault_authority_bump]];
    transfer_checked(
        CpiContext::new_with_signer(
            a.token_program.to_account_info(),
            TransferChecked {
                from: a.fee_vault.to_account_info(),
                mint: a.mint.to_account_info(),
                to: to.to_account_info(),
                authority: a.vault_authority.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        a.mint.decimals,
    )
}

pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let econ = ctx.accounts.config.economics;
    let i = ctx
        .accounts
        .config
        .fee_pool_index(&ctx.accounts.mint.key())
        .ok_or(GuardError::BadFeeMint)?;
    let mut pool = ctx.accounts.staking.pools[i];
    require!(pool.initialized, GuardError::PoolNotInitialized);
    require!(
        now >= pool.last_distribution_ts.saturating_add(econ.epoch_secs),
        GuardError::EpochNotElapsed
    );

    let amount = ctx.accounts.fee_vault.amount;
    let mut tracker = pool.tracker();
    let intensity = tracker.observe(amount, econ.revenue_alpha_ppm).tk()?;
    let params = econ.split();
    let parts = tk::split(amount, &params, intensity);
    let total_staked = ctx.accounts.staking.total_staked;

    let mut rewards = pool.reward_pool();
    rewards.add_rewards(parts.stakers, total_staked).tk()?;
    pool.store_reward_pool(&rewards);
    pool.store_tracker(&tracker);
    pool.last_distribution_ts = now;
    pool.total_fees = pool.total_fees.saturating_add(amount);
    pool.total_treasury = pool.total_treasury.saturating_add(parts.treasury);
    pool.total_buyback = pool.total_buyback.saturating_add(parts.buyback);
    pool.total_rewards = pool.total_rewards.saturating_add(parts.stakers);
    ctx.accounts.staking.pools[i] = pool;

    let a = &ctx.accounts;
    send(a, &a.treasury_account, parts.treasury)?;
    send(a, &a.buyback_vault, parts.buyback)?;
    send(a, &a.reward_vault, parts.stakers)?;

    emit!(Distributed {
        mint: a.mint.key(),
        amount,
        intensity_ppm: intensity,
        burn_bps: params.burn.burn_bps(intensity),
        treasury: parts.treasury,
        buyback: parts.buyback,
        stakers: parts.stakers,
        total_staked,
    });
    Ok(())
}
