#![allow(dead_code, clippy::unwrap_used, clippy::cast_possible_truncation)]

use ed25519_dalek::SigningKey;
use tenebra_chain::*;

pub const COIN: Amount = 1_000_000_000; // 9 decimals
pub const CHAIN: u64 = 7;
pub const T0: i64 = 1_800_000_000;
pub const DAY: i64 = 86_400;

pub fn params() -> ChainParams {
    ChainParams {
        chain_id: CHAIN,
        native_symbol: "TENEBRA".into(),
        native_decimals: 9,
        block_gas_limit: 100_000,
        block_gas_target: 50_000,
        initial_base_fee: 1_000,
        min_base_fee: 100,
        base_fee_change_denominator: 8,
        max_validators: 3,
        min_self_bond: 1_000 * COIN,
        max_commission_bps: 2_000,
        unbonding_secs: 21 * DAY,
        power_reduction: COIN,
        initial_issuance_bps: 800,
        issuance_floor_bps: 150,
        issuance_decay_bps_per_year: 100,
        max_issuance_interval_secs: 3_600,
        slash_double_sign_bps: 500,
        slash_downtime_bps: 1,
        downtime_window_blocks: 100,
        max_missed_in_window: 50,
        downtime_jail_secs: 600,
    }
}

pub fn key(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i; 32])
}

pub fn addr(i: u8) -> Address {
    Address(key(i).verifying_key().to_bytes())
}

pub fn cons(i: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[i.wrapping_add(100); 32])
        .verifying_key()
        .to_bytes()
}

/// Validators 1 and 2 at genesis (10k and 5k self-bond); users 10..=14 funded.
pub fn genesis() -> Genesis {
    let mut balances: Vec<(Address, AssetId, Amount)> = (10..=14)
        .map(|i| (addr(i), NATIVE, 100_000 * COIN))
        .collect();
    balances.push((addr(10), 1, 1_000_000)); // bridged USDC
    balances.push((addr(3), NATIVE, 50_000 * COIN)); // future validator
                                                     // Operators keep liquid coins to pay gas.
    balances.push((addr(1), NATIVE, 100 * COIN));
    balances.push((addr(2), NATIVE, 100 * COIN));
    Genesis {
        params: params(),
        genesis_time: T0,
        assets: vec![(1, "USDC".into())],
        balances,
        validators: vec![
            GenesisValidator {
                operator: addr(1),
                consensus_key: cons(1),
                self_bond: 10_000 * COIN,
                commission_bps: 1_000,
            },
            GenesisValidator {
                operator: addr(2),
                consensus_key: cons(2),
                self_bond: 5_000 * COIN,
                commission_bps: 500,
            },
        ],
    }
}

#[derive(Debug)]
pub struct Net {
    pub s: State,
    pub nonces: std::collections::HashMap<u8, u64>,
}

impl Net {
    pub fn new() -> Net {
        Net {
            s: State::genesis(&genesis()).unwrap().0,
            nonces: Default::default(),
        }
    }

    pub fn tx(&mut self, who: u8, kind: TxKind) -> Tx {
        let n = self.nonces.entry(who).or_insert(0);
        let tx = Tx::signed(CHAIN, &key(who), *n, u128::MAX, kind);
        *n += 1;
        tx
    }

    pub fn all_signed(&self) -> Vec<VoteInfo> {
        self.s
            .active_set()
            .iter()
            .map(|(k, p)| VoteInfo {
                consensus_key: *k,
                power: *p,
                signed: true,
            })
            .collect()
    }

    pub fn block_with(
        &mut self,
        dt: i64,
        txs: &[Tx],
        votes: Vec<VoteInfo>,
        evidence: Vec<Evidence>,
    ) -> BlockResult {
        let h = BlockHeader {
            height: self.s.height() + 1,
            time: self.s.time() + dt,
            last_votes: votes,
            evidence,
        };
        let r = self.s.finalize_block(&h, txs).unwrap();
        assert_eq!(
            self.s.counters().supply,
            self.s.native_holdings(),
            "supply invariant"
        );
        assert!(
            self.s.total_claimable() <= self.s.counters().reward_escrow,
            "escrow covers claims"
        );
        r
    }

    pub fn block(&mut self, txs: &[Tx]) -> BlockResult {
        let votes = self.all_signed();
        self.block_with(1, txs, votes, vec![])
    }
}
