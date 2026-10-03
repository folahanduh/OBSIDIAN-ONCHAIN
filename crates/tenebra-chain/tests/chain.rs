#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

mod common;

use common::*;
use tenebra_chain::*;

fn transfer(to: u8, amount: Amount) -> TxKind {
    TxKind::Transfer {
        to: addr(to),
        asset: NATIVE,
        amount,
    }
}

#[test]
fn genesis_sets_supply_and_validator_set() {
    let (s, updates) = State::genesis(&genesis()).unwrap();
    assert_eq!(
        s.counters().supply,
        5 * 100_000 * COIN + 50_000 * COIN + 200 * COIN + 15_000 * COIN
    );
    assert_eq!(s.counters().supply, s.native_holdings());
    let mut want = vec![
        ValidatorUpdate {
            consensus_key: cons(1),
            power: 10_000,
        },
        ValidatorUpdate {
            consensus_key: cons(2),
            power: 5_000,
        },
    ];
    want.sort_by_key(|u| u.consensus_key);
    assert_eq!(updates, want);
    assert_eq!(
        s.app_hash(),
        State::genesis(&genesis()).unwrap().0.app_hash(),
        "deterministic"
    );
}

#[test]
fn transfer_burns_gas_fee() {
    let mut n = Net::new();
    let supply0 = n.s.counters().supply;
    let t = n.tx(10, transfer(11, 5 * COIN));
    let r = n.block(&[t]);
    let fee = gas::TRANSFER as u128 * 1_000;
    assert_eq!(
        r.txs[0],
        TxOutcome {
            result: Ok(()),
            gas_used: gas::TRANSFER,
            fee_burned: fee
        }
    );
    assert_eq!(
        n.s.balance(addr(10), NATIVE),
        100_000 * COIN - 5 * COIN - fee
    );
    assert_eq!(n.s.balance(addr(11), NATIVE), 100_000 * COIN + 5 * COIN);
    assert_eq!(n.s.counters().total_fees_burned, fee);
    // Supply fell by the burned fee (plus rose by this block's issuance).
    assert_eq!(
        n.s.counters().supply,
        supply0 - fee + n.s.counters().total_issued
    );
}

#[test]
fn failed_tx_pays_fee_but_has_no_effect() {
    let mut n = Net::new();
    let t = n.tx(
        11,
        TxKind::Transfer {
            to: addr(12),
            asset: 1,
            amount: 1,
        },
    ); // 11 holds no USDC
    let r = n.block(&[t]);
    assert_eq!(r.txs[0].result, Err(TxError::InsufficientBalance));
    assert!(r.txs[0].fee_burned > 0);
    assert_eq!(n.s.nonce(addr(11)), 1, "nonce consumed");
    assert_eq!(n.s.balance(addr(12), 1), 0);
}

#[test]
fn invalid_txs_are_rejected_untouched() {
    let mut n = Net::new();
    let before = n.s.balance(addr(10), NATIVE);
    let mut bad_sig = n.tx(10, transfer(11, 1));
    bad_sig.sig[0] ^= 1;
    let wrong_chain = Tx::signed(CHAIN + 1, &key(10), 0, u128::MAX, transfer(11, 1));
    let low_cap = Tx::signed(CHAIN, &key(10), 0, 999, transfer(11, 1));
    let future_nonce = Tx::signed(CHAIN, &key(10), 5, u128::MAX, transfer(11, 1));
    let broke = Tx::signed(CHAIN, &key(50), 0, u128::MAX, transfer(11, 1));
    let r = n.block(&[bad_sig, wrong_chain, low_cap, future_nonce, broke]);
    let errs: Vec<_> = r.txs.iter().map(|o| o.result.unwrap_err()).collect();
    assert_eq!(
        errs,
        vec![
            TxError::BadSignature,
            TxError::WrongChain,
            TxError::FeeCapBelowBaseFee,
            TxError::BadNonce {
                expected: 0,
                got: 5
            },
            TxError::CannotPayFee
        ]
    );
    assert!(errs.iter().all(TxError::is_rejection));
    assert!(r.txs.iter().all(|o| o.fee_burned == 0));
    assert_eq!(n.s.balance(addr(10), NATIVE), before);
    assert_eq!(n.s.nonce(addr(10)), 0);
}

#[test]
fn base_fee_tracks_demand() {
    let mut n = Net::new();
    // 100 transfers = 100k gas = full block (2× target) ⇒ +12.5%.
    let txs: Vec<Tx> = (0..100).map(|_| n.tx(10, transfer(11, 1))).collect();
    n.block(&txs);
    assert_eq!(n.s.base_fee(), 1_125);
    // The block gas limit caps inclusion.
    let txs: Vec<Tx> = (0..101).map(|_| n.tx(12, transfer(11, 1))).collect();
    let r = n.block(&txs);
    assert_eq!(r.txs[100].result, Err(TxError::BlockGasExceeded));
    // Empty blocks fall 12.5% per block down to the floor.
    for _ in 0..40 {
        n.block(&[]);
    }
    assert_eq!(n.s.base_fee(), 100);
}

#[test]
fn delegate_unbond_and_complete() {
    let mut n = Net::new();
    let d = n.tx(
        10,
        TxKind::Delegate {
            validator: addr(2),
            amount: 6_000 * COIN,
        },
    );
    let r = n.block(&[d]);
    assert!(r.txs[0].result.is_ok());
    // Validator 2 now has 11k stake ⇒ power 11_000 (top of the set).
    assert_eq!(
        r.validator_updates,
        vec![ValidatorUpdate {
            consensus_key: cons(2),
            power: 11_000
        }]
    );
    assert_eq!(n.s.delegation_value(addr(10), addr(2)), 6_000 * COIN);

    let u = n.tx(
        10,
        TxKind::Undelegate {
            validator: addr(2),
            amount: 6_000 * COIN,
        },
    );
    let r = n.block(&[u]);
    assert!(r.txs[0].result.is_ok());
    assert_eq!(
        r.validator_updates,
        vec![ValidatorUpdate {
            consensus_key: cons(2),
            power: 5_000
        }]
    );
    let bal = n.s.balance(addr(10), NATIVE);
    n.block_with(21 * DAY - 1, &[], n.all_signed(), vec![]);
    assert_eq!(
        n.s.balance(addr(10), NATIVE),
        bal,
        "one second before completion"
    );
    let r = n.block(&[]);
    assert!(r.events.contains(&BlockEvent::UnbondingCompleted {
        delegator: addr(10),
        amount: 6_000 * COIN
    }));
    assert_eq!(n.s.balance(addr(10), NATIVE), bal + 6_000 * COIN);
}

#[test]
fn issuance_pays_signers_with_commission() {
    let mut n = Net::new();
    let d = n.tx(
        10,
        TxKind::Delegate {
            validator: addr(1),
            amount: 10_000 * COIN,
        },
    );
    n.block(&[d]); // validator 1: 20k stake (half from user 10)
    let escrow0 = n.s.counters().reward_escrow;
    let v2_commission0 = n.s.validator(addr(2)).unwrap().commission_owed;
    let v1_commission0 = n.s.validator(addr(1)).unwrap().commission_owed;
    let (op0, user0) = (
        n.s.pending_rewards(addr(1), addr(1)),
        n.s.pending_rewards(addr(10), addr(1)),
    );

    // One hour later, only validator 1 signed.
    let votes = vec![
        VoteInfo {
            consensus_key: cons(1),
            power: 20_000,
            signed: true,
        },
        VoteInfo {
            consensus_key: cons(2),
            power: 5_000,
            signed: false,
        },
    ];
    n.block_with(3_600, &[], votes, vec![]);
    let minted = n.s.counters().reward_escrow - escrow0;
    // ≈ supply × 8% × 1h/1y (rate has decayed by a negligible amount).
    let supply = n.s.counters().supply - minted;
    let expected = supply * 800 * 3_600 / (10_000 * 31_557_600);
    assert!(
        minted <= expected && minted + 2 > expected,
        "minted {minted} vs {expected}"
    );

    let commission = n.s.validator(addr(1)).unwrap().commission_owed - v1_commission0;
    assert!(commission > 0);
    assert_eq!(
        n.s.validator(addr(2)).unwrap().commission_owed,
        v2_commission0,
        "non-signer earns nothing"
    );
    // 10% commission, then operator and user 10 hold equal shares ⇒ equal rewards.
    assert!(commission.abs_diff(minted / 10) <= 1);
    let op = n.s.pending_rewards(addr(1), addr(1)) - op0;
    let user = n.s.pending_rewards(addr(10), addr(1)) - user0;
    assert!(op.abs_diff(user) <= 1);
    assert!(commission + op + user <= minted);
    let user = user + user0;

    let bal = n.s.balance(addr(10), NATIVE);
    let c = n.tx(10, TxKind::ClaimRewards { validator: addr(1) });
    let r = n.block(&[c]);
    assert!(r.txs[0].result.is_ok());
    assert!(n.s.balance(addr(10), NATIVE) + r.txs[0].fee_burned >= bal + user);
    let again = n.tx(10, TxKind::ClaimRewards { validator: addr(2) });
    assert_eq!(
        n.block(&[again]).txs[0].result,
        Err(TxError::NothingToClaim)
    );
}

#[test]
fn downtime_jails_and_unjail_restores() {
    let mut n = Net::new();
    let offline = |n: &Net| {
        n.s.active_set()
            .iter()
            .map(|(k, p)| VoteInfo {
                consensus_key: *k,
                power: *p,
                signed: *k != cons(2),
            })
            .collect::<Vec<_>>()
    };
    let tokens0 = n.s.validator(addr(2)).unwrap().tokens;
    let mut jailed_at = None;
    for _ in 0..60 {
        let votes = offline(&n);
        let r = n.block_with(1, &[], votes, vec![]);
        if r.validator_updates.contains(&ValidatorUpdate {
            consensus_key: cons(2),
            power: 0,
        }) {
            jailed_at = Some(n.s.height());
            assert!(r.events.iter().any(|e| matches!(
                e,
                BlockEvent::Slashed {
                    reason: SlashReason::Downtime,
                    ..
                }
            )));
            break;
        }
    }
    assert_eq!(jailed_at, Some(51), "jailed on the 51st miss");
    let v = n.s.validator(addr(2)).unwrap();
    assert!(v.jailed && !v.tombstoned);
    assert_eq!(v.tokens, tokens0 - tokens0 / 10_000, "0.01% slashed");

    let early = n.tx(2, TxKind::Unjail);
    assert_eq!(n.block(&[early]).txs[0].result, Err(TxError::StillJailed));
    n.block_with(600, &[], n.all_signed(), vec![]);
    let unjail = n.tx(2, TxKind::Unjail);
    let r = n.block(&[unjail]);
    assert!(r.txs[0].result.is_ok());
    assert_eq!(
        r.validator_updates,
        vec![ValidatorUpdate {
            consensus_key: cons(2),
            power: (tokens0 - tokens0 / 10_000) as u64 / COIN as u64
        }]
    );
}

#[test]
fn double_sign_slashes_stake_and_later_unbondings_and_tombstones() {
    let mut n = Net::new();
    let d = n.tx(
        11,
        TxKind::Delegate {
            validator: addr(1),
            amount: 10_000 * COIN,
        },
    );
    n.block(&[d]);
    let infraction_height = n.s.height() + 1;
    let u = n.tx(
        11,
        TxKind::Undelegate {
            validator: addr(1),
            amount: 2_000 * COIN,
        },
    );
    n.block(&[u]); // unbonding created at the infraction height ⇒ slashable
    let tokens = n.s.validator(addr(1)).unwrap().tokens;
    let supply = n.s.counters().supply;
    let user_value = n.s.delegation_value(addr(11), addr(1));

    let ev = vec![Evidence {
        consensus_key: cons(1),
        height: infraction_height,
    }];
    let r = n.block_with(1, &[], n.all_signed(), ev);
    let slashed = tokens / 20 + 2_000 * COIN / 20;
    assert!(r.events.contains(&BlockEvent::Slashed {
        validator: addr(1),
        amount: slashed,
        reason: SlashReason::DoubleSign
    }));
    assert!(r.validator_updates.contains(&ValidatorUpdate {
        consensus_key: cons(1),
        power: 0
    }));
    assert_eq!(n.s.counters().total_slashed, slashed);
    assert!(n.s.counters().supply < supply, "slashed coins are burned");
    let v = n.s.validator(addr(1)).unwrap();
    assert!(v.tombstoned);
    // Delegators share the loss pro-rata.
    let after = n.s.delegation_value(addr(11), addr(1));
    assert!(after.abs_diff(user_value - user_value / 20) <= 1);
    assert_eq!(n.s.unbondings().next().unwrap().amount, 1_900 * COIN);
    // Tombstoned validators can never return.
    let t = n.tx(1, TxKind::Unjail);
    assert_eq!(n.block(&[t]).txs[0].result, Err(TxError::Tombstoned));
}

#[test]
fn register_validator_rules_and_set_cap() {
    let mut n = Net::new();
    let dup_key = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: cons(1),
            commission_bps: 0,
            self_bond: 2_000 * COIN,
        },
    );
    let greedy = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: cons(3),
            commission_bps: 2_001,
            self_bond: 2_000 * COIN,
        },
    );
    let small = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: cons(3),
            commission_bps: 0,
            self_bond: 999 * COIN,
        },
    );
    let bad_key = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: [0; 32],
            commission_bps: 0,
            self_bond: 2_000 * COIN,
        },
    );
    let ok = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: cons(3),
            commission_bps: 0,
            self_bond: 2_000 * COIN,
        },
    );
    let again = n.tx(
        3,
        TxKind::RegisterValidator {
            consensus_key: cons(4),
            commission_bps: 0,
            self_bond: 2_000 * COIN,
        },
    );
    // 5 × 20k gas fills the 100k block; the 6th goes in the next block.
    let r = n.block(&[dup_key, greedy, small, bad_key, ok]);
    let res: Vec<_> = r.txs.iter().map(|o| o.result).collect();
    assert_eq!(
        res,
        vec![
            Err(TxError::ConsensusKeyInUse),
            Err(TxError::CommissionTooHigh),
            Err(TxError::SelfBondTooLow),
            Err(TxError::InvalidConsensusKey),
            Ok(())
        ]
    );
    assert_eq!(
        r.validator_updates,
        vec![ValidatorUpdate {
            consensus_key: cons(3),
            power: 2_000
        }]
    );
    assert_eq!(
        n.block(&[again]).txs[0].result,
        Err(TxError::ValidatorExists)
    );

    // A 4th validator with more stake evicts the smallest (max_validators = 3).
    let fund = n.tx(10, transfer(4, 20_000 * COIN));
    n.block(&[fund]);
    let reg = n.tx(
        4,
        TxKind::RegisterValidator {
            consensus_key: cons(4),
            commission_bps: 0,
            self_bond: 3_000 * COIN,
        },
    );
    let r = n.block(&[reg]);
    let mut want = vec![
        ValidatorUpdate {
            consensus_key: cons(3),
            power: 0,
        },
        ValidatorUpdate {
            consensus_key: cons(4),
            power: 3_000,
        },
    ];
    want.sort_by_key(|u| u.consensus_key);
    assert_eq!(r.validator_updates, want);
}

#[test]
fn operator_below_min_self_bond_leaves_the_set() {
    let mut n = Net::new();
    let u = n.tx(
        2,
        TxKind::Undelegate {
            validator: addr(2),
            amount: 4_500 * COIN,
        },
    );
    let r = n.block(&[u]);
    assert_eq!(
        r.validator_updates,
        vec![ValidatorUpdate {
            consensus_key: cons(2),
            power: 0
        }]
    );
}

#[test]
fn tx_wire_format_is_strict() {
    let t = Tx::signed(
        CHAIN,
        &key(10),
        3,
        77,
        TxKind::Delegate {
            validator: addr(1),
            amount: 5,
        },
    );
    let b = t.encode();
    assert_eq!(Tx::decode(&b).unwrap(), t);
    assert!(Tx::decode(&b).unwrap().verify().is_ok());
    let mut longer = b.clone();
    longer.push(0);
    assert_eq!(Tx::decode(&longer), Err(TxError::Malformed));
    assert_eq!(Tx::decode(&b[..b.len() - 1]), Err(TxError::Malformed));
}
