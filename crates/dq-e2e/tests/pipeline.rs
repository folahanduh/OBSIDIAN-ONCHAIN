//! Full order lifecycle:
//!
//! client: sign action (session key) → seal to sequencer key
//! sequencer: append ciphertext to ordering log (commit) → open → decode →
//!            authorize → match → fee (σ-adaptive) → encrypt fill notes
//! auditor: receive sealed epoch-key disclosure → scan chain → see own fills only

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::collections::HashMap;

use dq_auth::*;
use dq_engine::*;
use dq_privacy::*;
use dq_quant::*;
use dq_types::*;
use ed25519_dalek::SigningKey;
use getrandom::rand_core::UnwrapErr;
use getrandom::SysRng;

const CHAIN: u64 = 77;
const MARKET: MarketId = 1;
const T0: u64 = 20_300 * EPOCH_MS + 3_600_000;

fn spec() -> MarketSpec {
    MarketSpec {
        id: MARKET,
        tick_size: 10,
        lot_size: 1_000,
        min_qty: Qty(1),
        max_qty: Qty(1 << 32),
        max_price: Price(1 << 32),
    }
}

struct Trader {
    id: AccountId,
    master: SigningKey,
    session: SigningKey,
    seed: ViewingSeed,
    nonce: u64,
}

impl Trader {
    fn new(id: AccountId, b: u8) -> Self {
        Trader {
            id,
            master: SigningKey::from_bytes(&[b; 32]),
            session: SigningKey::from_bytes(&[b + 100; 32]),
            seed: ViewingSeed::from_bytes([b + 200; 32]),
            nonce: 0,
        }
    }

    fn sealed_action(
        &mut self,
        action: SessionAction,
        seq_pk: &SequencerPublicKey,
    ) -> SealedEnvelope {
        self.nonce += 1;
        let env = ActionEnvelope::new_signed(CHAIN, self.id, 1, self.nonce, action, &self.session);
        seal(
            SealPurpose::Order,
            &env.encode(),
            seq_pk,
            &mut UnwrapErr(SysRng),
        )
        .unwrap()
    }
}

struct Sequencer {
    key: SequencerKey,
    log: OrderingLog,
    auth: Authorizer,
    book: OrderBook,
    vol: EwmaVol,
    fees: FeeSchedule,
    viewing: HashMap<(AccountId, u64), EpochViewingPublicKey>,
    chain_notes: Vec<EncryptedNote>,
    fee_ledger: HashMap<AccountId, i128>,
}

impl Sequencer {
    fn new() -> Self {
        let mut rng = UnwrapErr(SysRng);
        Sequencer {
            key: SequencerKey::generate(&mut rng),
            log: OrderingLog::genesis(CHAIN),
            auth: Authorizer::new(CHAIN, [spec()], 8),
            book: OrderBook::new(BookConfig {
                spec: spec(),
                max_resting_orders: 10_000,
                max_open_orders_per_account: 100,
                max_match_steps: 32,
            })
            .unwrap(),
            vol: EwmaVol::new(61_604, 1_000_000).unwrap(),
            fees: FeeSchedule {
                maker_pips: -50,
                base_taker_pips: 250,
                min_taker_pips: 200,
                max_taker_pips: 1_000,
                sigma_ref: 1_000_000,
                slope_pips: 300,
            },
            viewing: HashMap::new(),
            chain_notes: vec![],
            fee_ledger: HashMap::new(),
        }
    }

    fn onboard(&mut self, t: &Trader) {
        self.auth
            .register_account(t.id, &t.master.verifying_key().to_bytes())
            .unwrap();
        let grant = SessionGrant {
            chain_id: CHAIN,
            account: t.id,
            session_pk: t.session.verifying_key().to_bytes(),
            perms: Permissions {
                markets: vec![MARKET],
                max_order_notional: 10_000_000_000,
                can_place: true,
                can_cancel: true,
            },
            valid_from_ms: T0 - 1,
            expires_at_ms: T0 + 86_400_000,
            grant_nonce: 1,
        };
        self.auth
            .install_session(&grant.sign(&t.master), T0)
            .unwrap();
        // Register viewing keys for today and tomorrow via a master action.
        let e0 = epoch_of(T0);
        let pks = vec![
            t.seed.epoch_key(t.id, e0).public().pk,
            t.seed.epoch_key(t.id, e0 + 1).public().pk,
        ];
        let env = MasterEnvelope::new_signed(
            CHAIN,
            t.id,
            1,
            MasterAction::RegisterViewingKeys {
                first_epoch: e0,
                pks,
            },
            &t.master,
        );
        if let MasterAction::RegisterViewingKeys { first_epoch, pks } =
            self.auth.authorize_master(&env).unwrap()
        {
            for (i, pk) in pks.into_iter().enumerate() {
                let vk = EpochViewingPublicKey {
                    account: t.id,
                    epoch: first_epoch + i as u64,
                    pk,
                };
                assert!(vk.is_valid());
                self.viewing.insert((t.id, vk.epoch), vk);
            }
        }
    }

    /// Phase 1: commit to ordering of ciphertexts. Returns receipts that the
    /// sequencer would sign and publish before decrypting anything.
    fn commit(&mut self, batch: &[SealedEnvelope]) -> Vec<SequenceReceipt> {
        batch.iter().map(|e| self.log.append(e)).collect()
    }

    /// Phase 2: decrypt and execute in committed order.
    fn execute(&mut self, batch: &[SealedEnvelope], now: u64) -> Vec<Event> {
        let mut rng = UnwrapErr(SysRng);
        let mut events = vec![];
        for sealed in batch {
            let Ok(plain) = open(SealPurpose::Order, sealed, &self.key) else {
                continue;
            };
            let Ok(env) = ActionEnvelope::decode(&plain) else {
                continue;
            };
            let Ok(ok) = self.auth.authorize(&env, now) else {
                continue;
            };
            let start = events.len();
            match ok.action {
                SessionAction::Place { .. } => {
                    let (_, req) = ok.order_request().unwrap();
                    self.book.place(req, &mut events);
                }
                SessionAction::Cancel { order_id, .. } => {
                    let _ = self.book.cancel(ok.account, order_id, &mut events);
                }
                SessionAction::CancelAll { .. } => {
                    self.book.cancel_all(ok.account, &mut events);
                }
            }
            for ev in &events[start..] {
                if let Event::Fill(f) = ev {
                    self.settle(f, now, &mut rng);
                }
            }
        }
        // Post-batch: sample mid for volatility (fixed cadence = per batch).
        if let (Some((b, _)), Some((a, _))) = (self.book.best_bid(), self.book.best_ask()) {
            self.vol.on_sample((b.0 + a.0) / 2).unwrap();
        }
        events
    }

    fn settle(&mut self, f: &Fill, now: u64, rng: &mut UnwrapErr<SysRng>) {
        let notional = spec().notional(f.price, f.qty);
        let taker_fee = self.fees.taker_fee(notional, self.vol.sigma()).unwrap() as i128;
        let maker_fee = self.fees.maker_fee(notional).unwrap();
        assert!(
            taker_fee + maker_fee >= 0,
            "venue never net-negative per fill"
        );
        for (acct, side, fee) in [
            (f.taker_owner, f.taker_side, taker_fee),
            (f.maker_owner, f.taker_side.opposite(), maker_fee),
        ] {
            *self.fee_ledger.entry(acct).or_default() += fee;
            let mut rseed = [0u8; 32];
            rng.fill_bytes(&mut rseed);
            let note = FillNote {
                account: acct,
                market: MARKET,
                side,
                price: f.price.0,
                qty: f.qty.0,
                fee,
                fill_seq: f.seq,
                ts_ms: now,
                rseed,
            };
            let pk = self.viewing[&(acct, epoch_of(now))];
            self.chain_notes
                .push(encrypt_note(&note, &pk, rng).unwrap());
        }
    }
}

use getrandom::rand_core::Rng as _;

fn place(side: Side, price: u64, qty: u64, tif: TimeInForce) -> SessionAction {
    SessionAction::Place {
        market: MARKET,
        client_id: 0,
        side,
        price: Price(price),
        qty: Qty(qty),
        tif,
        stp: StpMode::CancelTaker,
    }
}

#[test]
fn full_pipeline() {
    let mut seq = Sequencer::new();
    let (mut mm, mut taker, mut other) = (Trader::new(1, 1), Trader::new(2, 2), Trader::new(3, 3));
    for t in [&mm, &taker, &other] {
        seq.onboard(t);
    }
    let pk = seq.key.public();

    // Batch 1: maker quotes; another account rests a bid.
    let b1 = vec![
        mm.sealed_action(place(Side::Ask, 10_010, 5, TimeInForce::PostOnly), &pk),
        mm.sealed_action(place(Side::Ask, 10_020, 5, TimeInForce::PostOnly), &pk),
        mm.sealed_action(place(Side::Bid, 9_990, 5, TimeInForce::PostOnly), &pk),
        other.sealed_action(place(Side::Bid, 9_980, 2, TimeInForce::Gtc), &pk),
    ];
    let r1 = seq.commit(&b1);
    assert!(r1.iter().all(SequenceReceipt::verify));
    seq.execute(&b1, T0);

    // Batch 2: taker sweeps one level and part of the next; a replayed
    // ciphertext from batch 1 and a garbage envelope are dropped.
    let b2 = vec![
        taker.sealed_action(place(Side::Bid, 10_020, 7, TimeInForce::Ioc), &pk),
        b1[0].clone(),
        SealedEnvelope {
            epk: [9; 32],
            ct: vec![0; 256],
            tag: [0; 16],
        },
    ];
    seq.commit(&b2);
    let ev = seq.execute(&b2, T0 + 1_000);
    let fills: Vec<_> = ev
        .iter()
        .filter_map(|e| {
            if let Event::Fill(f) = e {
                Some((f.price.0, f.qty.0))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(fills, vec![(10_010, 5), (10_020, 2)]);
    assert_eq!(seq.book.best_ask(), Some((Price(10_020), 3)));
    assert_eq!(seq.log.height(), 7);

    // Anyone can recompute the ordering head from published ciphertexts.
    let all: Vec<_> = b1.iter().chain(&b2).cloned().collect();
    assert_eq!(OrderingLog::replay(CHAIN, &all).head(), seq.log.head());

    // 2 fills × 2 counterparties = 4 notes on chain, opaque to the public.
    assert_eq!(seq.chain_notes.len(), 4);

    // Taker discloses today's epoch to a CEX over a sealed channel.
    let mut rng = UnwrapErr(SysRng);
    let cex = SequencerKey::generate(&mut rng);
    let e0 = epoch_of(T0);
    let disc = ComplianceDisclosure::from_seed_range(&taker.seed, taker.id, e0, e0).unwrap();
    let wire = seal(
        SealPurpose::Disclosure,
        &disc.encode(),
        &cex.public(),
        &mut rng,
    )
    .unwrap();
    let at_cex =
        ComplianceDisclosure::decode(&open(SealPurpose::Disclosure, &wire, &cex).unwrap()).unwrap();
    let seen = at_cex.scan(&seq.chain_notes);
    assert_eq!(seen.len(), 2, "CEX sees exactly the taker's two fills");
    assert!(seen
        .iter()
        .all(|n| n.account == taker.id && n.side == Side::Bid));
    let total_fee: i128 = seen.iter().map(|n| n.fee).sum();
    assert_eq!(total_fee, seq.fee_ledger[&taker.id]);
    assert!(total_fee > 0);

    // The maker's notes are invisible to the taker's disclosure, and the
    // uninvolved account has no notes at all.
    let mm_disc = ComplianceDisclosure::from_seed_range(&mm.seed, mm.id, e0, e0 + 1).unwrap();
    assert_eq!(mm_disc.scan(&seq.chain_notes).len(), 2);
    let other_disc =
        ComplianceDisclosure::from_seed_range(&other.seed, other.id, e0, e0 + 1).unwrap();
    assert!(other_disc.scan(&seq.chain_notes).is_empty());

    // Kill switch via session key, then the session can be revoked by master.
    let b3 = vec![mm.sealed_action(SessionAction::CancelAll { market: MARKET }, &pk)];
    seq.commit(&b3);
    let ev = seq.execute(&b3, T0 + 2_000);
    assert_eq!(ev.len(), 2);
    assert_eq!(seq.book.open_orders(mm.id), 0);
    seq.book.validate().unwrap();

    let revoke = MasterEnvelope::new_signed(
        CHAIN,
        mm.id,
        2,
        MasterAction::RevokeSessions { min_grant_nonce: 2 },
        &mm.master,
    );
    seq.auth.authorize_master(&revoke).unwrap();
    let b4 = vec![mm.sealed_action(place(Side::Ask, 10_050, 1, TimeInForce::Gtc), &pk)];
    seq.commit(&b4);
    assert!(
        seq.execute(&b4, T0 + 3_000).is_empty(),
        "revoked session is inert"
    );
}
