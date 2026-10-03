//! The swap guard: `pre_swap` → router instruction → `post_swap`.
//!
//! Enforced on-chain, independent of the router's own logic:
//! - the router is on the allowlist and the transaction has exactly the
//!   guarded shape (see `introspection`);
//! - the signer is not sanctioned;
//! - the input spent never exceeds `max_in` and the output received, net of
//!   fees, is at least `min_out` (slippage limit) — measured from token
//!   balance deltas, so it holds whatever the router does internally;
//! - the protocol fee is computed from the staker's discount curve and
//!   transferred to the fee vault in the same transaction.

use anchor_lang::prelude::*;
use anchor_lang::Discriminator;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use tenebra_tokenomics as tk;

use crate::error::{GuardError, TkResultExt};
use crate::introspection;
use crate::state::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct PreSwapArgs {
    /// Maximum input the router may take (fee excluded). Input-side fees are
    /// charged on this amount.
    pub max_in: u64,
    /// Minimum output the user must end up with, after any output-side fee.
    pub min_out: u64,
    pub markup_ppm: u32,
    pub fee_side: FeeSide,
}

#[derive(Accounts)]
pub struct PreSwap<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [DENYLIST_SEED], bump = denylist.bump)]
    pub denylist: Box<Account<'info, Denylist>>,
    #[account(seeds = [POSITION_SEED, user.key().as_ref()], bump = position.bump)]
    pub position: Option<Box<Account<'info, Position>>>,
    #[account(mut, token::authority = user)]
    pub in_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(token::authority = user)]
    pub out_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = user, space = 8 + SwapSession::INIT_SPACE, seeds = [SESSION_SEED, user.key().as_ref()], bump)]
    pub session: Box<Account<'info, SwapSession>>,
    #[account(mint::token_program = token_program)]
    pub fee_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [FEE_VAULT_SEED, fee_mint.key().as_ref()], bump)]
    pub fee_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: address-constrained to the instructions sysvar.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

pub fn pre_swap(ctx: Context<PreSwap>, args: PreSwapArgs) -> Result<()> {
    let a = &ctx.accounts;
    let cfg = &a.config;
    require!(!cfg.paused, GuardError::Paused);
    require!(!a.denylist.contains(&a.user.key()), GuardError::Denylisted);
    require_keys_neq!(a.in_account.mint, a.out_account.mint, GuardError::SameMint);

    let fee_side_mint = match args.fee_side {
        FeeSide::Input => a.in_account.mint,
        FeeSide::Output => a.out_account.mint,
    };
    require_keys_eq!(fee_side_mint, a.fee_mint.key(), GuardError::BadFeeMint);
    require!(
        cfg.fee_pool_index(&fee_side_mint).is_some(),
        GuardError::BadFeeMint
    );

    let session_key = a.session.key();
    let (ixs, current) = introspection::load_all(&a.instructions.to_account_info())?;
    introspection::check_layout(
        &ixs,
        current,
        &crate::ID,
        crate::instruction::PreSwap::DISCRIMINATOR,
        crate::instruction::PostSwap::DISCRIMINATOR,
        cfg.routers(),
        &session_key,
    )?;

    let now = Clock::get()?.unix_timestamp;
    let econ = cfg.economics;
    let effective = a
        .position
        .as_ref()
        .map_or(0, |p| p.stake().effective(now, &econ.timing()));
    // Prices the fee and validates the markup bounds in one place.
    let quote = econ
        .fee_policy()
        .quote(args.max_in, args.markup_ppm, effective)
        .map_err(|_| error!(GuardError::MarkupOutOfBounds))?;
    let waived = quote.fee == 0 && econ.zero_fee_tier != 0 && quote.tier >= econ.zero_fee_tier;

    let mut input_fee_paid = 0;
    if args.fee_side == FeeSide::Input && quote.fee > 0 {
        transfer_checked(
            CpiContext::new(
                a.token_program.to_account_info(),
                TransferChecked {
                    from: a.in_account.to_account_info(),
                    mint: a.fee_mint.to_account_info(),
                    to: a.fee_vault.to_account_info(),
                    authority: a.user.to_account_info(),
                },
            ),
            quote.fee,
            a.fee_mint.decimals,
        )?;
        input_fee_paid = quote.fee;
    }

    let a = ctx.accounts;
    a.in_account.reload()?;
    let s = &mut a.session;
    s.user = a.user.key();
    s.in_account = a.in_account.key();
    s.out_account = a.out_account.key();
    s.fee_mint = a.fee_mint.key();
    s.fee_side = args.fee_side;
    s.in_balance_start = a.in_account.amount;
    s.out_balance_start = a.out_account.amount;
    s.max_in = args.max_in;
    s.min_out = args.min_out;
    s.markup_ppm = args.markup_ppm;
    s.discount_ppm = quote.discount_ppm;
    s.tier = quote.tier;
    s.fee_waived = waived;
    s.input_fee_paid = input_fee_paid;
    s.bump = ctx.bumps.session;
    Ok(())
}

#[derive(Accounts)]
pub struct PostSwap<'info> {
    // Order matters: `introspection::POST_SWAP_SESSION_INDEX` points at `session`.
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut, close = user, has_one = user, has_one = in_account, has_one = out_account, has_one = fee_mint,
        seeds = [SESSION_SEED, user.key().as_ref()], bump = session.bump,
    )]
    pub session: Box<Account<'info, SwapSession>>,
    pub in_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub out_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub fee_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [FEE_VAULT_SEED, fee_mint.key().as_ref()], bump)]
    pub fee_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: address-constrained to the instructions sysvar.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}

#[event]
pub struct SwapGuarded {
    pub user: Pubkey,
    pub in_mint: Pubkey,
    pub out_mint: Pubkey,
    pub spent: u64,
    pub received: u64,
    pub fee: u64,
    pub fee_mint: Pubkey,
    pub tier: u8,
    pub discount_ppm: u32,
}

pub fn post_swap(ctx: Context<PostSwap>) -> Result<()> {
    let a = &ctx.accounts;
    require!(
        introspection::is_top_level(
            &a.instructions.to_account_info(),
            &crate::ID,
            crate::instruction::PostSwap::DISCRIMINATOR
        )?,
        GuardError::NotTopLevel
    );
    let s = &a.session;

    // Balances are re-read fresh for this instruction, i.e. after the router ran.
    let spent = s.in_balance_start.saturating_sub(a.in_account.amount);
    require!(spent <= s.max_in, GuardError::OverSpend);
    let received = a
        .out_account
        .amount
        .checked_sub(s.out_balance_start)
        .ok_or(GuardError::OutputDecreased)?;

    let fee = match s.fee_side {
        FeeSide::Input => s.input_fee_paid,
        FeeSide::Output if s.fee_waived => 0,
        FeeSide::Output => tk::fee_amount(received, s.markup_ppm, s.discount_ppm).tk()?,
    };
    let net = match s.fee_side {
        FeeSide::Input => received,
        FeeSide::Output => received - fee.min(received),
    };
    require!(net >= s.min_out, GuardError::SlippageExceeded);

    if s.fee_side == FeeSide::Output && fee > 0 {
        transfer_checked(
            CpiContext::new(
                a.token_program.to_account_info(),
                TransferChecked {
                    from: a.out_account.to_account_info(),
                    mint: a.fee_mint.to_account_info(),
                    to: a.fee_vault.to_account_info(),
                    authority: a.user.to_account_info(),
                },
            ),
            fee,
            a.fee_mint.decimals,
        )?;
    }

    emit!(SwapGuarded {
        user: s.user,
        in_mint: a.in_account.mint,
        out_mint: a.out_account.mint,
        spent,
        received: net,
        fee,
        fee_mint: s.fee_mint,
        tier: s.tier,
        discount_ppm: s.discount_ppm,
    });
    Ok(())
}
