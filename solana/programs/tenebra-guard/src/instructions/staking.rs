//! Staking with warmup (no flash-stake discounts) and cooldown (no reward
//! sniping). Rewards are paid from real fee revenue, per fee mint.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::error::{GuardError, TkResultExt};
use crate::state::*;

/// Bring every initialized pool's checkpoint up to date for `pos` at its
/// current stake. Must run before any change to the stake amount.
fn settle_all(staking: &StakingState, pos: &mut Position) -> Result<()> {
    let staked = pos.stake().staked();
    for (i, pool) in staking.pools.iter().enumerate() {
        if !pool.initialized {
            continue;
        }
        let mut cp = pos.checkpoint(i);
        pool.reward_pool().settle(&mut cp, staked).tk()?;
        pos.store_checkpoint(i, &cp);
    }
    Ok(())
}

#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Account<'info, StakingState>,
    #[account(init, payer = owner, space = 8 + Position::INIT_SPACE, seeds = [POSITION_SEED, owner.key().as_ref()], bump)]
    pub position: Account<'info, Position>,
    pub system_program: Program<'info, System>,
}

pub fn open_position(ctx: Context<OpenPosition>) -> Result<()> {
    let p = &mut ctx.accounts.position;
    p.owner = ctx.accounts.owner.key();
    p.bump = ctx.bumps.position;
    // Start every checkpoint at the current index: no claim on past rewards.
    settle_all(&ctx.accounts.staking, p)
}

#[derive(Accounts)]
pub struct Stake<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Account<'info, StakingState>,
    #[account(mut, seeds = [POSITION_SEED, owner.key().as_ref()], bump = position.bump, has_one = owner)]
    pub position: Account<'info, Position>,
    #[account(seeds = [DENYLIST_SEED], bump = denylist.bump)]
    pub denylist: Account<'info, Denylist>,
    #[account(address = config.stake_mint)]
    pub stake_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = stake_mint, token::authority = owner)]
    pub owner_tokens: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [STAKE_VAULT_SEED], bump)]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
    require!(!ctx.accounts.config.paused, GuardError::Paused);
    require!(
        !ctx.accounts.denylist.contains(&ctx.accounts.owner.key()),
        GuardError::Denylisted
    );
    let now = Clock::get()?.unix_timestamp;
    let timing = ctx.accounts.config.economics.timing();
    let pos = &mut ctx.accounts.position;
    settle_all(&ctx.accounts.staking, pos)?;
    let mut s = pos.stake();
    s.deposit(amount, now, &timing).tk()?;
    pos.store_stake(&s);
    let st = &mut ctx.accounts.staking;
    st.total_staked = st
        .total_staked
        .checked_add(amount)
        .ok_or(GuardError::Overflow)?;

    let a = &ctx.accounts;
    transfer_checked(
        CpiContext::new(
            a.token_program.to_account_info(),
            TransferChecked {
                from: a.owner_tokens.to_account_info(),
                mint: a.stake_mint.to_account_info(),
                to: a.stake_vault.to_account_info(),
                authority: a.owner.to_account_info(),
            },
        ),
        amount,
        a.stake_mint.decimals,
    )
}

#[derive(Accounts)]
pub struct RequestUnstake<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Account<'info, StakingState>,
    #[account(mut, seeds = [POSITION_SEED, owner.key().as_ref()], bump = position.bump, has_one = owner)]
    pub position: Account<'info, Position>,
}

pub fn request_unstake(ctx: Context<RequestUnstake>, amount: u64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let timing = ctx.accounts.config.economics.timing();
    let pos = &mut ctx.accounts.position;
    settle_all(&ctx.accounts.staking, pos)?;
    let mut s = pos.stake();
    s.request_unstake(amount, now, &timing).tk()?;
    pos.store_stake(&s);
    let st = &mut ctx.accounts.staking;
    st.total_staked = st
        .total_staked
        .checked_sub(amount)
        .ok_or(GuardError::Overflow)?;
    Ok(())
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [POSITION_SEED, owner.key().as_ref()], bump = position.bump, has_one = owner)]
    pub position: Account<'info, Position>,
    #[account(address = config.stake_mint)]
    pub stake_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = stake_mint)]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [STAKE_VAULT_SEED], bump)]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: vault authority PDA.
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let pos = &mut ctx.accounts.position;
    let mut s = pos.stake();
    let amount = s.withdraw(now).tk()?;
    pos.store_stake(&s);

    let a = &ctx.accounts;
    let seeds: &[&[u8]] = &[VAULT_AUTHORITY_SEED, &[a.config.vault_authority_bump]];
    transfer_checked(
        CpiContext::new_with_signer(
            a.token_program.to_account_info(),
            TransferChecked {
                from: a.stake_vault.to_account_info(),
                mint: a.stake_mint.to_account_info(),
                to: a.destination.to_account_info(),
                authority: a.vault_authority.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        a.stake_mint.decimals,
    )
}

#[derive(Accounts)]
pub struct Claim<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Account<'info, StakingState>,
    #[account(mut, seeds = [POSITION_SEED, owner.key().as_ref()], bump = position.bump, has_one = owner)]
    pub position: Account<'info, Position>,
    #[account(seeds = [DENYLIST_SEED], bump = denylist.bump)]
    pub denylist: Account<'info, Denylist>,
    pub reward_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = reward_mint)]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [REWARD_VAULT_SEED, reward_mint.key().as_ref()], bump)]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: vault authority PDA.
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn claim(ctx: Context<Claim>) -> Result<()> {
    let a = &ctx.accounts;
    require!(!a.denylist.contains(&a.owner.key()), GuardError::Denylisted);
    let i = a
        .config
        .fee_pool_index(&a.reward_mint.key())
        .ok_or(GuardError::BadFeeMint)?;
    let pool = a.staking.pools[i];
    require!(pool.initialized, GuardError::PoolNotInitialized);

    let pos = &mut ctx.accounts.position;
    let mut cp = pos.checkpoint(i);
    let amount = pool
        .reward_pool()
        .claim(&mut cp, pos.stake().staked())
        .tk()?;
    pos.store_checkpoint(i, &cp);
    require!(amount > 0, GuardError::NothingToClaim);

    let a = &ctx.accounts;
    let seeds: &[&[u8]] = &[VAULT_AUTHORITY_SEED, &[a.config.vault_authority_bump]];
    transfer_checked(
        CpiContext::new_with_signer(
            a.token_program.to_account_info(),
            TransferChecked {
                from: a.reward_vault.to_account_info(),
                mint: a.reward_mint.to_account_info(),
                to: a.destination.to_account_info(),
                authority: a.vault_authority.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        a.reward_mint.decimals,
    )
}
