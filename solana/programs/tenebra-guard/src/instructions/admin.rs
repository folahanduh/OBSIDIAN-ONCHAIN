use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::error::GuardError;
use crate::state::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitParams {
    pub compliance_authority: Pubkey,
    pub treasury: Pubkey,
    pub routers: Vec<Pubkey>,
    pub fee_mints: Vec<Pubkey>,
    pub economics: Economics,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    #[account(init, payer = admin, space = 8 + StakingState::INIT_SPACE, seeds = [STAKING_SEED], bump)]
    pub staking: Account<'info, StakingState>,
    #[account(init, payer = admin, space = 8 + Denylist::INIT_SPACE, seeds = [DENYLIST_SEED], bump)]
    pub denylist: Account<'info, Denylist>,
    pub stake_mint: InterfaceAccount<'info, Mint>,
    /// CHECK: PDA that owns every vault; holds no data.
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump)]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(
        init, payer = admin, seeds = [STAKE_VAULT_SEED], bump,
        token::mint = stake_mint, token::authority = vault_authority, token::token_program = token_program,
    )]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

fn copy_into<const N: usize>(src: &[Pubkey]) -> Result<([Pubkey; N], u8)> {
    require!(src.len() <= N, GuardError::InvalidConfig);
    let mut out = [Pubkey::default(); N];
    out[..src.len()].copy_from_slice(src);
    // Duplicates would let one entry shadow another's slot.
    for (i, a) in src.iter().enumerate() {
        require!(!src[..i].contains(a), GuardError::InvalidConfig);
    }
    Ok((out, src.len() as u8))
}

pub fn initialize(ctx: Context<Initialize>, p: InitParams) -> Result<()> {
    p.economics.validate().map_err(GuardError::from)?;
    require!(
        !p.fee_mints.is_empty() && !p.routers.is_empty(),
        GuardError::InvalidConfig
    );
    let (routers, router_count) = copy_into::<MAX_ROUTERS>(&p.routers)?;
    let (fee_mints, fee_mint_count) = copy_into::<MAX_FEE_MINTS>(&p.fee_mints)?;

    let c = &mut ctx.accounts.config;
    c.admin = ctx.accounts.admin.key();
    c.compliance_authority = p.compliance_authority;
    c.treasury = p.treasury;
    c.stake_mint = ctx.accounts.stake_mint.key();
    c.paused = false;
    c.routers = routers;
    c.router_count = router_count;
    c.fee_mints = fee_mints;
    c.fee_mint_count = fee_mint_count;
    c.economics = p.economics;
    c.bump = ctx.bumps.config;
    c.vault_authority_bump = ctx.bumps.vault_authority;

    let s = &mut ctx.accounts.staking;
    s.total_staked = 0;
    s.pools = [PoolState::default(); MAX_FEE_MINTS];
    for (pool, mint) in s.pools.iter_mut().zip(&p.fee_mints) {
        pool.mint = *mint;
    }
    s.bump = ctx.bumps.staking;

    ctx.accounts.denylist.entries = Vec::new();
    ctx.accounts.denylist.bump = ctx.bumps.denylist;
    Ok(())
}

#[derive(Accounts)]
pub struct InitFeePool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [STAKING_SEED], bump = staking.bump)]
    pub staking: Account<'info, StakingState>,
    /// Must be a classic SPL Token mint (no transfer-fee extensions in fee paths).
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: vault authority PDA.
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(
        init, payer = admin, seeds = [FEE_VAULT_SEED, mint.key().as_ref()], bump,
        token::mint = mint, token::authority = vault_authority, token::token_program = token_program,
    )]
    pub fee_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = admin, seeds = [REWARD_VAULT_SEED, mint.key().as_ref()], bump,
        token::mint = mint, token::authority = vault_authority, token::token_program = token_program,
    )]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = admin, seeds = [BUYBACK_VAULT_SEED, mint.key().as_ref()], bump,
        token::mint = mint, token::authority = vault_authority, token::token_program = token_program,
    )]
    pub buyback_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn init_fee_pool(ctx: Context<InitFeePool>) -> Result<()> {
    let i = ctx
        .accounts
        .config
        .fee_pool_index(&ctx.accounts.mint.key())
        .ok_or(GuardError::BadFeeMint)?;
    let pool = &mut ctx.accounts.staking.pools[i];
    require!(!pool.initialized, GuardError::InvalidConfig);
    pool.initialized = true;
    pool.last_distribution_ts = Clock::get()?.unix_timestamp;
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateParams {
    pub admin: Option<Pubkey>,
    pub compliance_authority: Option<Pubkey>,
    pub treasury: Option<Pubkey>,
    pub paused: Option<bool>,
    pub routers: Option<Vec<Pubkey>>,
    pub economics: Option<Economics>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
}

/// Fee mints and the stake mint are deliberately immutable: changing them
/// would orphan vault balances.
pub fn update_config(ctx: Context<UpdateConfig>, p: UpdateParams) -> Result<()> {
    let c = &mut ctx.accounts.config;
    if let Some(e) = p.economics {
        e.validate().map_err(GuardError::from)?;
        c.economics = e;
    }
    if let Some(r) = p.routers {
        require!(!r.is_empty(), GuardError::InvalidConfig);
        (c.routers, c.router_count) = copy_into::<MAX_ROUTERS>(&r)?;
    }
    if let Some(v) = p.paused {
        c.paused = v;
    }
    if let Some(v) = p.treasury {
        c.treasury = v;
    }
    if let Some(v) = p.compliance_authority {
        c.compliance_authority = v;
    }
    if let Some(v) = p.admin {
        c.admin = v;
    }
    Ok(())
}

#[derive(Accounts)]
pub struct UpdateDenylist<'info> {
    pub compliance_authority: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = compliance_authority)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [DENYLIST_SEED], bump = denylist.bump)]
    pub denylist: Account<'info, Denylist>,
}

pub fn update_denylist(
    ctx: Context<UpdateDenylist>,
    add: Vec<Pubkey>,
    remove: Vec<Pubkey>,
) -> Result<()> {
    let list = &mut ctx.accounts.denylist.entries;
    list.retain(|k| !remove.contains(k));
    for k in add {
        if let Err(pos) = list.binary_search(&k) {
            require!(list.len() < DENYLIST_CAPACITY, GuardError::DenylistFull);
            list.insert(pos, k);
        }
    }
    Ok(())
}
