//! Randomised blocks: supply conservation, escrow solvency and determinism.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

mod common;

use common::*;
use proptest::prelude::*;
use tenebra_chain::*;

#[derive(Clone, Debug)]
enum Op {
    Transfer(u8, u8, u64),
    Delegate(u8, u8, u64),
    Undelegate(u8, u8, u64),
    Claim(u8, u8),
    Unjail(u8),
}

#[derive(Clone, Debug)]
struct Blk {
    dt: i64,
    ops: Vec<Op>,
    offline: Vec<bool>,
    evidence: Option<u8>,
}

fn arb_op() -> impl Strategy<Value = Op> {
    let user = 10u8..=14;
    let val = 1u8..=2;
    prop_oneof![
        (user.clone(), user.clone(), 1u64..10_000).prop_map(|(a, b, x)| Op::Transfer(a, b, x)),
        (user.clone(), val.clone(), 1u64..20_000).prop_map(|(a, v, x)| Op::Delegate(a, v, x)),
        (
            prop_oneof![user.clone(), val.clone()],
            val.clone(),
            1u64..20_000
        )
            .prop_map(|(a, v, x)| Op::Undelegate(a, v, x)),
        (prop_oneof![user, val.clone()], val.clone()).prop_map(|(a, v)| Op::Claim(a, v)),
        val.prop_map(Op::Unjail),
    ]
}

fn arb_block() -> impl Strategy<Value = Blk> {
    (
        prop_oneof![8 => 1i64..10, 1 => 0i64..1, 1 => 600i64..30 * DAY],
        prop::collection::vec(arb_op(), 0..12),
        prop::collection::vec(prop::bool::weighted(0.15), 3),
        prop::option::weighted(0.03, 1u8..=2),
    )
        .prop_map(|(dt, ops, offline, evidence)| Blk {
            dt,
            ops,
            offline,
            evidence,
        })
}

fn run(blocks: &[Blk]) -> Vec<[u8; 32]> {
    let mut n = Net::new();
    let mut hashes = vec![];
    for b in blocks {
        let txs: Vec<Tx> = b
            .ops
            .iter()
            .map(|op| match *op {
                Op::Transfer(a, to, x) => n.tx(
                    a,
                    TxKind::Transfer {
                        to: addr(to),
                        asset: NATIVE,
                        amount: x as u128 * COIN / 100,
                    },
                ),
                Op::Delegate(a, v, x) => n.tx(
                    a,
                    TxKind::Delegate {
                        validator: addr(v),
                        amount: x as u128 * COIN / 10,
                    },
                ),
                Op::Undelegate(a, v, x) => n.tx(
                    a,
                    TxKind::Undelegate {
                        validator: addr(v),
                        amount: x as u128 * COIN / 10,
                    },
                ),
                Op::Claim(a, v) => n.tx(a, TxKind::ClaimRewards { validator: addr(v) }),
                Op::Unjail(v) => n.tx(v, TxKind::Unjail),
            })
            .collect();
        let votes: Vec<VoteInfo> =
            n.s.active_set()
                .iter()
                .enumerate()
                .map(|(i, (k, p))| VoteInfo {
                    consensus_key: *k,
                    power: *p,
                    signed: !b.offline[i % 3],
                })
                .collect();
        let ev = b
            .evidence
            .map(|v| {
                vec![Evidence {
                    consensus_key: cons(v),
                    height: n.s.height(),
                }]
            })
            .unwrap_or_default();
        // Rejected txs (e.g. a nonce gap after a rejection) are fine; invariants
        // are asserted inside `block_with`.
        let r = n.block_with(b.dt, &txs, votes, ev);
        // Resync local nonces with the chain after rejections.
        for i in 1..=14u8 {
            n.nonces.insert(i, n.s.nonce(addr(i)));
        }
        hashes.push(r.app_hash);
    }
    hashes
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn invariants_hold_and_nodes_agree(blocks in prop::collection::vec(arb_block(), 1..60)) {
        let a = run(&blocks);
        let b = run(&blocks);
        prop_assert_eq!(a, b, "two nodes replaying the same blocks diverged");
    }
}
