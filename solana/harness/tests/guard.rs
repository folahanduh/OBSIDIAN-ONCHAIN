//! Runtime tests: the guard program runs natively inside `solana-program-test`
//! next to the real (embedded) SPL Token program. A mock router registered at
//! Jupiter V6's address moves tokens like a swap would, with amounts chosen by
//! each test, so every guard check is exercised against real token balances.

#![allow(clippy::too_many_arguments)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::entrypoint::ProgramResult;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_lang::solana_program::program_option::COption;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::solana_program::sysvar;
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use solana_program_test::{processor, BanksClientError, ProgramTest, ProgramTestContext};
use solana_sdk::account::Account as SdkAccount;
use solana_sdk::instruction::InstructionError;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::{Transaction, TransactionError};
use tenebra_guard::error::GuardError;
use tenebra_guard::instructions::{InitParams, PreSwapArgs, UpdateParams};
use tenebra_guard::introspection::POST_SWAP_SESSION_INDEX;
use tenebra_guard::state::*;
use tenebra_tokenomics as tk;

const JUPITER: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const UNLISTED_ROUTER: Pubkey = pubkey!("5ZiE3vAkrdXBgyFL7KqG3RoEGBws4CjRcXVbABDLZTgx");
const DAY: i64 = 86_400;
const TOKEN: u64 = 1_000_000; // 6 decimals

// ------------------------------------------------------------ native entries

fn guard_entry(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    // Anchor's entrypoint ties the slice and AccountInfo lifetimes together.
    // The harness keeps these accounts alive for the whole call, so widening
    // the lifetimes for the duration of this call is sound.
    let accounts: &[AccountInfo<'static>] = unsafe { std::mem::transmute(accounts) };
    tenebra_guard::entry(program_id, accounts, data)
}

/// Mock router: takes `amount_in` from the user, pays `amount_out` from its pool.
/// Data: amount_in (u64 LE) ‖ amount_out (u64 LE).
fn mock_router(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [user, user_in, user_out, pool_in, pool_out, pool_auth, token_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let amount_in = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let amount_out = u64::from_le_bytes(data[8..16].try_into().unwrap());
    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            user_in.key,
            pool_in.key,
            user.key,
            &[],
            amount_in,
        )?,
        &[
            user_in.clone(),
            pool_in.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;
    let (_, bump) = Pubkey::find_program_address(&[b"pool"], program_id);
    invoke_signed(
        &spl_token::instruction::transfer(
            token_program.key,
            pool_out.key,
            user_out.key,
            pool_auth.key,
            &[],
            amount_out,
        )?,
        &[
            pool_out.clone(),
            user_out.clone(),
            pool_auth.clone(),
            token_program.clone(),
        ],
        &[&[b"pool", &[bump]]],
    )?;
    Ok(())
}

// ------------------------------------------------------------ fixtures

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &tenebra_guard::ID).0
}

fn add_mint(pt: &mut ProgramTest, mint: Pubkey, authority: Pubkey) {
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint {
        mint_authority: COption::Some(authority),
        supply: u64::MAX / 2,
        decimals: 6,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut data);
    pt.add_account(
        mint,
        SdkAccount {
            lamports: 1_000_000_000,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    );
}

fn add_token_account(
    pt: &mut ProgramTest,
    address: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
) {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account {
        mint,
        owner,
        amount,
        state: spl_token::state::AccountState::Initialized,
        ..Default::default()
    }
    .pack_into_slice(&mut data);
    pt.add_account(
        address,
        SdkAccount {
            lamports: 1_000_000_000,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    );
}

fn economics() -> Economics {
    Economics {
        markup_min_ppm: 1_500,
        markup_max_ppm: 8_000,
        discount_cap_ppm: 500_000,
        stake_for_cap: 1_000_000 * TOKEN,
        tier1: 10_000 * TOKEN,
        tier2: 100_000 * TOKEN,
        tier3: 1_000_000 * TOKEN,
        zero_fee_tier: 0,
        warmup_secs: DAY,
        cooldown_secs: 7 * DAY,
        treasury_bps: 5_000,
        burn_base_bps: 5_000,
        burn_min_bps: 3_000,
        burn_max_bps: 9_000,
        burn_slope_bps: 2_000,
        revenue_alpha_ppm: 200_000,
        epoch_secs: DAY,
    }
}

struct Env {
    ctx: ProgramTestContext,
    admin: Keypair,
    compliance: Keypair,
    user: Keypair,
    usdc: Pubkey,
    xmint: Pubkey,
    stake_mint: Pubkey,
    user_usdc: Pubkey,
    user_x: Pubkey,
    user_stake: Pubkey,
    pool_usdc: Pubkey,
    pool_x: Pubkey,
    treasury_usdc: Pubkey,
}

impl Env {
    async fn new() -> Env {
        let mut pt = ProgramTest::new("tenebra_guard", tenebra_guard::ID, processor!(guard_entry));
        pt.prefer_bpf(false);
        pt.add_program("mock_router", JUPITER, processor!(mock_router));
        pt.add_program("unlisted_router", UNLISTED_ROUTER, processor!(mock_router));

        let (admin, compliance, user, treasury) = (
            Keypair::new(),
            Keypair::new(),
            Keypair::new(),
            Keypair::new(),
        );
        let (usdc, xmint, stake_mint) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        for m in [usdc, xmint, stake_mint] {
            add_mint(&mut pt, m, admin.pubkey());
        }
        let pool_auth = Pubkey::find_program_address(&[b"pool"], &JUPITER).0;
        let ids: Vec<Pubkey> = (0..6).map(|_| Pubkey::new_unique()).collect();
        add_token_account(&mut pt, ids[0], usdc, user.pubkey(), 10_000_000 * TOKEN);
        add_token_account(&mut pt, ids[1], xmint, user.pubkey(), 10_000_000 * TOKEN);
        add_token_account(
            &mut pt,
            ids[2],
            stake_mint,
            user.pubkey(),
            2_000_000 * TOKEN,
        );
        add_token_account(&mut pt, ids[3], usdc, pool_auth, 100_000_000 * TOKEN);
        add_token_account(&mut pt, ids[4], xmint, pool_auth, 100_000_000 * TOKEN);
        add_token_account(&mut pt, ids[5], usdc, treasury.pubkey(), 0);
        for k in [&admin, &compliance, &user] {
            pt.add_account(
                k.pubkey(),
                SdkAccount {
                    lamports: 100_000_000_000,
                    ..Default::default()
                },
            );
        }

        let ctx = pt.start_with_context().await;
        let mut env = Env {
            ctx,
            admin,
            compliance,
            user,
            usdc,
            xmint,
            stake_mint,
            user_usdc: ids[0],
            user_x: ids[1],
            user_stake: ids[2],
            pool_usdc: ids[3],
            pool_x: ids[4],
            treasury_usdc: ids[5],
        };
        let treasury_key = treasury.pubkey();
        env.initialize(treasury_key).await;
        env
    }

    async fn send(
        &mut self,
        ixs: &[Instruction],
        signers: &[&Keypair],
    ) -> std::result::Result<(), BanksClientError> {
        let bh = self.ctx.get_new_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(ixs, Some(&signers[0].pubkey()), signers, bh);
        self.ctx.banks_client.process_transaction(tx).await
    }

    async fn initialize(&mut self, treasury: Pubkey) {
        let accounts = tenebra_guard::accounts::Initialize {
            admin: self.admin.pubkey(),
            config: pda(&[CONFIG_SEED]),
            staking: pda(&[STAKING_SEED]),
            denylist: pda(&[DENYLIST_SEED]),
            stake_mint: self.stake_mint,
            vault_authority: pda(&[VAULT_AUTHORITY_SEED]),
            stake_vault: pda(&[STAKE_VAULT_SEED]),
            token_program: spl_token::ID,
            system_program: anchor_lang::system_program::ID,
        };
        let data = tenebra_guard::instruction::Initialize {
            params: InitParams {
                compliance_authority: self.compliance.pubkey(),
                treasury,
                routers: vec![JUPITER],
                fee_mints: vec![self.usdc],
                economics: economics(),
            },
        };
        let ix = Instruction {
            program_id: tenebra_guard::ID,
            accounts: accounts.to_account_metas(None),
            data: data.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin]).await.unwrap();

        let accounts = tenebra_guard::accounts::InitFeePool {
            admin: self.admin.pubkey(),
            config: pda(&[CONFIG_SEED]),
            staking: pda(&[STAKING_SEED]),
            mint: self.usdc,
            vault_authority: pda(&[VAULT_AUTHORITY_SEED]),
            fee_vault: pda(&[FEE_VAULT_SEED, self.usdc.as_ref()]),
            reward_vault: pda(&[REWARD_VAULT_SEED, self.usdc.as_ref()]),
            buyback_vault: pda(&[BUYBACK_VAULT_SEED, self.usdc.as_ref()]),
            token_program: spl_token::ID,
            system_program: anchor_lang::system_program::ID,
        };
        let ix = Instruction {
            program_id: tenebra_guard::ID,
            accounts: accounts.to_account_metas(None),
            data: tenebra_guard::instruction::InitFeePool {}.data(),
        };
        self.send(&[ix], &[&admin]).await.unwrap();
    }

    async fn balance(&mut self, account: Pubkey) -> u64 {
        let a = self
            .ctx
            .banks_client
            .get_account(account)
            .await
            .unwrap()
            .unwrap();
        spl_token::state::Account::unpack(&a.data).unwrap().amount
    }

    async fn staking(&mut self) -> StakingState {
        let a = self
            .ctx
            .banks_client
            .get_account(pda(&[STAKING_SEED]))
            .await
            .unwrap()
            .unwrap();
        StakingState::try_deserialize(&mut a.data.as_slice()).unwrap()
    }

    async fn advance(&mut self, secs: i64) {
        let mut clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp += secs;
        self.ctx.set_sysvar(&clock);
    }

    /// Build pre_swap → router → post_swap. `buy` = USDC in, X out; else X in, USDC out.
    fn swap_ixs(
        &self,
        router: Pubkey,
        buy: bool,
        args: PreSwapArgs,
        router_in: u64,
        router_out: u64,
        with_position: bool,
    ) -> Vec<Instruction> {
        let user = self.user.pubkey();
        let (in_acct, out_acct, pool_in, pool_out) = if buy {
            (self.user_usdc, self.user_x, self.pool_usdc, self.pool_x)
        } else {
            (self.user_x, self.user_usdc, self.pool_x, self.pool_usdc)
        };
        let session = pda(&[SESSION_SEED, user.as_ref()]);
        let fee_vault = pda(&[FEE_VAULT_SEED, self.usdc.as_ref()]);
        let pre = tenebra_guard::accounts::PreSwap {
            user,
            config: pda(&[CONFIG_SEED]),
            denylist: pda(&[DENYLIST_SEED]),
            position: with_position.then(|| pda(&[POSITION_SEED, user.as_ref()])),
            in_account: in_acct,
            out_account: out_acct,
            session,
            fee_mint: self.usdc,
            fee_vault,
            token_program: spl_token::ID,
            instructions: sysvar::instructions::ID,
            system_program: anchor_lang::system_program::ID,
        };
        let mut route_data = router_in.to_le_bytes().to_vec();
        route_data.extend_from_slice(&router_out.to_le_bytes());
        let pool_auth = Pubkey::find_program_address(&[b"pool"], &router).0;
        let route = Instruction {
            program_id: router,
            accounts: vec![
                AccountMeta::new_readonly(user, true),
                AccountMeta::new(in_acct, false),
                AccountMeta::new(out_acct, false),
                AccountMeta::new(pool_in, false),
                AccountMeta::new(pool_out, false),
                AccountMeta::new_readonly(pool_auth, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: route_data,
        };
        let post = tenebra_guard::accounts::PostSwap {
            user,
            session,
            in_account: in_acct,
            out_account: out_acct,
            fee_mint: self.usdc,
            fee_vault,
            token_program: spl_token::ID,
            instructions: sysvar::instructions::ID,
        };
        vec![
            Instruction {
                program_id: tenebra_guard::ID,
                accounts: pre.to_account_metas(None),
                data: tenebra_guard::instruction::PreSwap { args }.data(),
            },
            route,
            Instruction {
                program_id: tenebra_guard::ID,
                accounts: post.to_account_metas(None),
                data: tenebra_guard::instruction::PostSwap {}.data(),
            },
        ]
    }

    async fn swap(
        &mut self,
        buy: bool,
        args: PreSwapArgs,
        router_in: u64,
        router_out: u64,
    ) -> std::result::Result<(), BanksClientError> {
        let ixs = self.swap_ixs(JUPITER, buy, args, router_in, router_out, false);
        let user = self.user.insecure_clone();
        self.send(&ixs, &[&user]).await
    }

    fn position_ix(&self, ix: impl InstructionData, accounts: impl ToAccountMetas) -> Instruction {
        Instruction {
            program_id: tenebra_guard::ID,
            accounts: accounts.to_account_metas(None),
            data: ix.data(),
        }
    }

    async fn open_and_stake(&mut self, amount: u64) {
        let user = self.user.pubkey();
        let open = self.position_ix(
            tenebra_guard::instruction::OpenPosition {},
            tenebra_guard::accounts::OpenPosition {
                owner: user,
                staking: pda(&[STAKING_SEED]),
                position: pda(&[POSITION_SEED, user.as_ref()]),
                system_program: anchor_lang::system_program::ID,
            },
        );
        let stake = self.stake_ix(amount);
        let k = self.user.insecure_clone();
        self.send(&[open, stake], &[&k]).await.unwrap();
    }

    fn stake_ix(&self, amount: u64) -> Instruction {
        let user = self.user.pubkey();
        self.position_ix(
            tenebra_guard::instruction::Stake { amount },
            tenebra_guard::accounts::Stake {
                owner: user,
                config: pda(&[CONFIG_SEED]),
                staking: pda(&[STAKING_SEED]),
                position: pda(&[POSITION_SEED, user.as_ref()]),
                denylist: pda(&[DENYLIST_SEED]),
                stake_mint: self.stake_mint,
                owner_tokens: self.user_stake,
                stake_vault: pda(&[STAKE_VAULT_SEED]),
                token_program: spl_token::ID,
            },
        )
    }

    async fn distribute(&mut self) -> std::result::Result<(), BanksClientError> {
        let ix = self.position_ix(
            tenebra_guard::instruction::Distribute {},
            tenebra_guard::accounts::Distribute {
                config: pda(&[CONFIG_SEED]),
                staking: pda(&[STAKING_SEED]),
                mint: self.usdc,
                fee_vault: pda(&[FEE_VAULT_SEED, self.usdc.as_ref()]),
                reward_vault: pda(&[REWARD_VAULT_SEED, self.usdc.as_ref()]),
                buyback_vault: pda(&[BUYBACK_VAULT_SEED, self.usdc.as_ref()]),
                treasury_account: self.treasury_usdc,
                vault_authority: pda(&[VAULT_AUTHORITY_SEED]),
                token_program: spl_token::ID,
            },
        );
        // Anyone can crank: use the compliance key as an arbitrary payer.
        let k = self.compliance.insecure_clone();
        self.send(&[ix], &[&k]).await
    }
}

fn code(e: GuardError) -> u32 {
    e as u32 + anchor_lang::error::ERROR_CODE_OFFSET
}

fn custom_error(r: std::result::Result<(), BanksClientError>) -> u32 {
    match r {
        Err(BanksClientError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(c),
        )))
        | Err(BanksClientError::SimulationError {
            err: TransactionError::InstructionError(_, InstructionError::Custom(c)),
            ..
        }) => c,
        other => panic!("expected custom program error, got {other:?}"),
    }
}

fn args(max_in: u64, min_out: u64, side: FeeSide) -> PreSwapArgs {
    PreSwapArgs {
        max_in,
        min_out,
        markup_ppm: 3_000,
        fee_side: side,
    }
}

// ------------------------------------------------------------------- tests

#[test]
fn session_index_constant_matches_account_order() {
    let metas = tenebra_guard::accounts::PostSwap {
        user: Pubkey::new_unique(),
        session: Pubkey::new_unique(),
        in_account: Pubkey::new_unique(),
        out_account: Pubkey::new_unique(),
        fee_mint: Pubkey::new_unique(),
        fee_vault: Pubkey::new_unique(),
        token_program: spl_token::ID,
        instructions: sysvar::instructions::ID,
    };
    let session = metas.session;
    assert_eq!(
        metas.to_account_metas(None)[POST_SWAP_SESSION_INDEX].pubkey,
        session
    );
}

#[tokio::test]
async fn input_side_fee_is_collected_and_limits_hold() {
    let mut env = Env::new().await;
    let (u0, x0) = (
        env.balance(env.user_usdc).await,
        env.balance(env.user_x).await,
    );
    // $100k USDC → X at 0.30% markup, fee on input.
    let max_in = 100_000 * TOKEN;
    env.swap(
        true,
        args(max_in, 49_000 * TOKEN, FeeSide::Input),
        max_in,
        50_000 * TOKEN,
    )
    .await
    .unwrap();
    let fee = tk::fee_amount(max_in, 3_000, 0).unwrap();
    assert_eq!(fee, 300 * TOKEN);
    assert_eq!(env.balance(env.user_usdc).await, u0 - max_in - fee);
    assert_eq!(env.balance(env.user_x).await, x0 + 50_000 * TOKEN);
    assert_eq!(
        env.balance(pda(&[FEE_VAULT_SEED, env.usdc.as_ref()])).await,
        fee
    );
    // Session account is closed again (rent returned).
    assert!(env
        .ctx
        .banks_client
        .get_account(pda(&[SESSION_SEED, env.user.pubkey().as_ref()]))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn output_side_fee_is_collected() {
    let mut env = Env::new().await;
    let u0 = env.balance(env.user_usdc).await;
    // Sell X for $20,000.000001 USDC; fee rounds up.
    let out = 20_000 * TOKEN + 1;
    env.swap(
        false,
        args(40_000 * TOKEN, 19_900 * TOKEN, FeeSide::Output),
        40_000 * TOKEN,
        out,
    )
    .await
    .unwrap();
    let fee = tk::fee_amount(out, 3_000, 0).unwrap();
    assert_eq!(fee, 60_000_001);
    assert_eq!(env.balance(env.user_usdc).await, u0 + out - fee);
    assert_eq!(
        env.balance(pda(&[FEE_VAULT_SEED, env.usdc.as_ref()])).await,
        fee
    );
}

#[tokio::test]
async fn slippage_limit_reverts_everything() {
    let mut env = Env::new().await;
    let (u0, x0) = (
        env.balance(env.user_usdc).await,
        env.balance(env.user_x).await,
    );
    // Router delivers 1 unit less than the minimum (e.g. the user was sandwiched).
    let r = env
        .swap(
            true,
            args(1_000 * TOKEN, 500 * TOKEN, FeeSide::Input),
            1_000 * TOKEN,
            500 * TOKEN - 1,
        )
        .await;
    assert_eq!(custom_error(r), code(GuardError::SlippageExceeded));
    assert_eq!(
        env.balance(env.user_usdc).await,
        u0,
        "fee and swap rolled back atomically"
    );
    assert_eq!(env.balance(env.user_x).await, x0);

    // Output-side: min_out applies after the fee.
    let out = 1_000 * TOKEN;
    let net = out - tk::fee_amount(out, 3_000, 0).unwrap();
    let r = env
        .swap(
            false,
            args(10 * TOKEN, net + 1, FeeSide::Output),
            10 * TOKEN,
            out,
        )
        .await;
    assert_eq!(custom_error(r), code(GuardError::SlippageExceeded));
    env.swap(
        false,
        args(10 * TOKEN, net, FeeSide::Output),
        10 * TOKEN,
        out,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn router_cannot_overspend() {
    let mut env = Env::new().await;
    let r = env
        .swap(
            true,
            args(1_000 * TOKEN, 1, FeeSide::Input),
            1_000 * TOKEN + 1,
            10 * TOKEN,
        )
        .await;
    assert_eq!(custom_error(r), code(GuardError::OverSpend));
}

#[tokio::test]
async fn unlisted_router_rejected() {
    let mut env = Env::new().await;
    let ixs = env.swap_ixs(
        UNLISTED_ROUTER,
        true,
        args(100, 1, FeeSide::Input),
        100,
        10,
        false,
    );
    let k = env.user.insecure_clone();
    let r = env.send(&ixs, &[&k]).await;
    assert_eq!(custom_error(r), code(GuardError::RouterNotAllowed));
}

#[tokio::test]
async fn post_swap_cannot_be_stripped() {
    let mut env = Env::new().await;
    // Fee on the output side, so without post_swap no fee would ever be taken.
    let mut ixs = env.swap_ixs(
        JUPITER,
        false,
        args(100 * TOKEN, 1, FeeSide::Output),
        100 * TOKEN,
        10 * TOKEN,
        false,
    );
    ixs.pop(); // attacker drops post_swap to dodge fee + slippage checks
    let k = env.user.insecure_clone();
    let r = env.send(&ixs, &[&k]).await;
    assert_eq!(custom_error(r), code(GuardError::BadLayout));
}

#[tokio::test]
async fn markup_bounds_and_fee_mint_rules() {
    let mut env = Env::new().await;
    let mut a = args(100, 1, FeeSide::Input);
    a.markup_ppm = 8_001;
    let r = env.swap(true, a, 100, 10).await;
    assert_eq!(custom_error(r), code(GuardError::MarkupOutOfBounds));
    // Fees are only taken in configured fee mints: X is not one.
    let r = env.swap(true, args(100, 1, FeeSide::Output), 100, 10).await;
    assert_eq!(custom_error(r), code(GuardError::BadFeeMint));
    // Same mint in and out is refused.
    let mut ixs = env.swap_ixs(JUPITER, true, args(100, 1, FeeSide::Input), 100, 10, false);
    ixs[0].accounts[5].pubkey = env.user_usdc; // out_account := in_account
    let k = env.user.insecure_clone();
    assert_eq!(
        custom_error(env.send(&ixs, &[&k]).await),
        code(GuardError::SameMint)
    );
    let _ = env.xmint;
}

#[tokio::test]
async fn denylist_blocks_swaps_and_pause_blocks_everything() {
    let mut env = Env::new().await;
    let add_ix = env.position_ix(
        tenebra_guard::instruction::UpdateDenylist {
            add: vec![env.user.pubkey()],
            remove: vec![],
        },
        tenebra_guard::accounts::UpdateDenylist {
            compliance_authority: env.compliance.pubkey(),
            config: pda(&[CONFIG_SEED]),
            denylist: pda(&[DENYLIST_SEED]),
        },
    );
    // Only the compliance authority may edit the list.
    let mut forged = add_ix.clone();
    forged.accounts[0].pubkey = env.admin.pubkey();
    let admin = env.admin.insecure_clone();
    assert!(env.send(&[forged], &[&admin]).await.is_err());

    let c = env.compliance.insecure_clone();
    env.send(&[add_ix], &[&c]).await.unwrap();
    let r = env.swap(true, args(100, 1, FeeSide::Input), 100, 10).await;
    assert_eq!(custom_error(r), code(GuardError::Denylisted));

    let remove_ix = env.position_ix(
        tenebra_guard::instruction::UpdateDenylist {
            add: vec![],
            remove: vec![env.user.pubkey()],
        },
        tenebra_guard::accounts::UpdateDenylist {
            compliance_authority: env.compliance.pubkey(),
            config: pda(&[CONFIG_SEED]),
            denylist: pda(&[DENYLIST_SEED]),
        },
    );
    env.send(&[remove_ix], &[&c]).await.unwrap();

    let pause = env.position_ix(
        tenebra_guard::instruction::UpdateConfig {
            params: UpdateParams {
                admin: None,
                compliance_authority: None,
                treasury: None,
                paused: Some(true),
                routers: None,
                economics: None,
            },
        },
        tenebra_guard::accounts::UpdateConfig {
            admin: env.admin.pubkey(),
            config: pda(&[CONFIG_SEED]),
        },
    );
    env.send(&[pause], &[&admin]).await.unwrap();
    let r = env.swap(true, args(100, 1, FeeSide::Input), 100, 10).await;
    assert_eq!(custom_error(r), code(GuardError::Paused));
}

#[tokio::test]
async fn staking_discount_requires_warmup() {
    let mut env = Env::new().await;
    env.open_and_stake(1_000_000 * TOKEN).await; // exactly the cap stake
    let vault = pda(&[FEE_VAULT_SEED, env.usdc.as_ref()]);
    let k = env.user.insecure_clone();

    // Same day: flash-staked tokens earn no discount.
    let ixs = env.swap_ixs(
        JUPITER,
        true,
        args(10_000 * TOKEN, 1, FeeSide::Input),
        10_000 * TOKEN,
        1,
        true,
    );
    env.send(&ixs, &[&k]).await.unwrap();
    assert_eq!(env.balance(vault).await, 30 * TOKEN);

    env.advance(DAY).await;
    let ixs = env.swap_ixs(
        JUPITER,
        true,
        args(10_000 * TOKEN, 1, FeeSide::Input),
        10_000 * TOKEN,
        2,
        true,
    );
    env.send(&ixs, &[&k]).await.unwrap();
    assert_eq!(
        env.balance(vault).await,
        30 * TOKEN + 15 * TOKEN,
        "50% discount after warmup"
    );
}

#[tokio::test]
async fn revenue_split_and_real_yield_claim() {
    let mut env = Env::new().await;
    env.open_and_stake(500_000 * TOKEN).await;
    // Generate $3,000 of fees: $1M swap at 0.30%.
    env.swap(
        true,
        args(1_000_000 * TOKEN, 1, FeeSide::Input),
        1_000_000 * TOKEN,
        1,
    )
    .await
    .unwrap();
    let fees = 3_000 * TOKEN;

    assert_eq!(
        custom_error(env.distribute().await),
        code(GuardError::EpochNotElapsed)
    );
    env.advance(DAY).await;
    env.distribute().await.unwrap();

    let expected = tk::split(fees, &economics().split(), 1_000_000); // first epoch: intensity 1.0
    assert_eq!(env.balance(env.treasury_usdc).await, expected.treasury);
    assert_eq!(
        env.balance(pda(&[BUYBACK_VAULT_SEED, env.usdc.as_ref()]))
            .await,
        expected.buyback
    );
    assert_eq!(
        env.balance(pda(&[REWARD_VAULT_SEED, env.usdc.as_ref()]))
            .await,
        expected.stakers
    );
    assert_eq!(
        env.balance(pda(&[FEE_VAULT_SEED, env.usdc.as_ref()])).await,
        0
    );
    assert_eq!(
        (expected.treasury, expected.buyback, expected.stakers),
        (1_500 * TOKEN, 750 * TOKEN, 750 * TOKEN)
    );
    let st = env.staking().await;
    assert_eq!(st.pools[0].total_fees, fees);

    // The only staker claims the whole staker share, in USDC.
    let user = env.user.pubkey();
    let claim = env.position_ix(
        tenebra_guard::instruction::Claim {},
        tenebra_guard::accounts::Claim {
            owner: user,
            config: pda(&[CONFIG_SEED]),
            staking: pda(&[STAKING_SEED]),
            position: pda(&[POSITION_SEED, user.as_ref()]),
            denylist: pda(&[DENYLIST_SEED]),
            reward_mint: env.usdc,
            destination: env.user_usdc,
            reward_vault: pda(&[REWARD_VAULT_SEED, env.usdc.as_ref()]),
            vault_authority: pda(&[VAULT_AUTHORITY_SEED]),
            token_program: spl_token::ID,
        },
    );
    let before = env.balance(env.user_usdc).await;
    let k = env.user.insecure_clone();
    env.send(&[claim.clone()], &[&k]).await.unwrap();
    assert_eq!(env.balance(env.user_usdc).await, before + expected.stakers);
    assert_eq!(
        custom_error(env.send(&[claim], &[&k]).await),
        code(GuardError::NothingToClaim)
    );
}

#[tokio::test]
async fn unstake_cooldown_then_withdraw() {
    let mut env = Env::new().await;
    env.open_and_stake(1_000 * TOKEN).await;
    let user = env.user.pubkey();
    let req = env.position_ix(
        tenebra_guard::instruction::RequestUnstake {
            amount: 400 * TOKEN,
        },
        tenebra_guard::accounts::RequestUnstake {
            owner: user,
            config: pda(&[CONFIG_SEED]),
            staking: pda(&[STAKING_SEED]),
            position: pda(&[POSITION_SEED, user.as_ref()]),
        },
    );
    let withdraw = env.position_ix(
        tenebra_guard::instruction::Withdraw {},
        tenebra_guard::accounts::Withdraw {
            owner: user,
            config: pda(&[CONFIG_SEED]),
            position: pda(&[POSITION_SEED, user.as_ref()]),
            stake_mint: env.stake_mint,
            destination: env.user_stake,
            stake_vault: pda(&[STAKE_VAULT_SEED]),
            vault_authority: pda(&[VAULT_AUTHORITY_SEED]),
            token_program: spl_token::ID,
        },
    );
    let k = env.user.insecure_clone();
    env.send(&[req], &[&k]).await.unwrap();
    assert_eq!(env.staking().await.total_staked, 600 * TOKEN);
    assert_eq!(
        custom_error(env.send(&[withdraw.clone()], &[&k]).await),
        code(GuardError::CooldownActive)
    );
    env.advance(7 * DAY).await;
    let before = env.balance(env.user_stake).await;
    env.send(&[withdraw], &[&k]).await.unwrap();
    assert_eq!(env.balance(env.user_stake).await, before + 400 * TOKEN);
}

#[tokio::test]
async fn non_admin_cannot_reconfigure() {
    let mut env = Env::new().await;
    let hijack = env.position_ix(
        tenebra_guard::instruction::UpdateConfig {
            params: UpdateParams {
                admin: Some(env.user.pubkey()),
                compliance_authority: None,
                treasury: None,
                paused: None,
                routers: Some(vec![UNLISTED_ROUTER]),
                economics: None,
            },
        },
        tenebra_guard::accounts::UpdateConfig {
            admin: env.user.pubkey(),
            config: pda(&[CONFIG_SEED]),
        },
    );
    let k = env.user.insecure_clone();
    assert!(env.send(&[hijack], &[&k]).await.is_err());
}
