//! Differential test: the optimized book must emit exactly the same event
//! stream as a deliberately naive O(n) reference model, for arbitrary op
//! sequences. The reference implements FOK by *simulating on a clone* rather
//! than by a dry-run walk, so the two FOK paths are independent.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use proptest::prelude::*;
use tenebra_engine::*;
use tenebra_types::{AccountId, MarketSpec, OrderId, Price, Qty, Side};

fn cfg() -> BookConfig {
    BookConfig {
        spec: MarketSpec {
            id: 7,
            tick_size: 1,
            lot_size: 1,
            min_qty: Qty(1),
            max_qty: Qty(50),
            max_price: Price(200),
        },
        max_resting_orders: 24,
        max_open_orders_per_account: 9,
        max_match_steps: 6,
    }
}

#[derive(Clone, Debug)]
struct RefOrder {
    id: OrderId,
    owner: AccountId,
    client_id: u64,
    side: Side,
    price: Price,
    remaining: Qty,
    seq: u64,
    acct_seq: u64,
}

#[derive(Clone, Debug)]
struct RefBook {
    cfg: BookConfig,
    orders: Vec<RefOrder>,
    next_id: OrderId,
    fill_seq: u64,
    seq: u64,
}

fn better(side: Side, a: Price, b: Price) -> bool {
    match side {
        Side::Bid => a > b,
        Side::Ask => a < b,
    }
}

fn crosses(taker: Side, limit: Price, maker: Price) -> bool {
    match taker {
        Side::Bid => maker <= limit,
        Side::Ask => maker >= limit,
    }
}

impl RefBook {
    fn new(cfg: BookConfig) -> Self {
        RefBook {
            cfg,
            orders: vec![],
            next_id: 1,
            fill_seq: 0,
            seq: 0,
        }
    }

    fn best_opp(&self, r: &OrderRequest) -> Option<usize> {
        let opp = r.side.opposite();
        let mut best: Option<usize> = None;
        for (i, o) in self.orders.iter().enumerate() {
            if o.side != opp || !crosses(r.side, r.price, o.price) {
                continue;
            }
            best = match best {
                None => Some(i),
                Some(j) => {
                    let b = &self.orders[j];
                    if better(opp, o.price, b.price) || (o.price == b.price && o.seq < b.seq) {
                        Some(i)
                    } else {
                        Some(j)
                    }
                }
            };
        }
        best
    }

    fn cancel_ev(o: &RefOrder, reason: CancelReason) -> Event {
        Event::Cancelled {
            order_id: o.id,
            owner: o.owner,
            side: o.side,
            price: o.price,
            remaining: o.remaining,
            reason,
        }
    }

    /// Returns (remaining, taker_cancelled).
    fn run_match(&mut self, id: OrderId, r: &OrderRequest, out: &mut Vec<Event>) -> (Qty, bool) {
        let mut rem = r.qty;
        let mut steps = 0;
        let taker_cancel = |rem: Qty, reason| Event::Cancelled {
            order_id: id,
            owner: r.owner,
            side: r.side,
            price: r.price,
            remaining: rem,
            reason,
        };
        while rem.0 > 0 {
            let Some(i) = self.best_opp(r) else { break };
            if steps == self.cfg.max_match_steps {
                out.push(taker_cancel(rem, CancelReason::MatchLimit));
                return (rem, true);
            }
            steps += 1;
            if self.orders[i].owner == r.owner {
                match r.stp {
                    StpMode::CancelTaker => {
                        out.push(taker_cancel(rem, CancelReason::SelfTradeTaker));
                        return (rem, true);
                    }
                    StpMode::CancelMaker | StpMode::CancelBoth => {
                        let m = self.orders.remove(i);
                        out.push(Self::cancel_ev(&m, CancelReason::SelfTradeMaker));
                        if r.stp == StpMode::CancelBoth {
                            out.push(taker_cancel(rem, CancelReason::SelfTradeTaker));
                            return (rem, true);
                        }
                        continue;
                    }
                }
            }
            let m = &mut self.orders[i];
            let q = rem.0.min(m.remaining.0);
            rem.0 -= q;
            m.remaining.0 -= q;
            self.fill_seq += 1;
            out.push(Event::Fill(Fill {
                seq: self.fill_seq,
                maker_order: m.id,
                maker_owner: m.owner,
                taker_order: id,
                taker_owner: r.owner,
                taker_side: r.side,
                price: m.price,
                qty: Qty(q),
                maker_remaining: m.remaining,
                taker_remaining: rem,
            }));
            if m.remaining.0 == 0 {
                self.orders.remove(i);
            }
        }
        (rem, false)
    }

    fn place(&mut self, r: OrderRequest, out: &mut Vec<Event>) {
        let s = &self.cfg.spec;
        let reject = |reason| Event::Rejected {
            owner: r.owner,
            client_id: r.client_id,
            reason,
        };
        let invalid = if r.qty.0 == 0 {
            Some(RejectReason::ZeroQty)
        } else if r.qty < s.min_qty {
            Some(RejectReason::QtyBelowMin)
        } else if r.qty > s.max_qty {
            Some(RejectReason::QtyAboveMax)
        } else if r.price.0 == 0 {
            Some(RejectReason::ZeroPrice)
        } else if r.price > s.max_price {
            Some(RejectReason::PriceAboveMax)
        } else {
            None
        };
        if let Some(reason) = invalid {
            out.push(reject(reason));
            return;
        }
        if r.tif == TimeInForce::PostOnly && self.best_opp(&r).is_some() {
            out.push(reject(RejectReason::PostOnlyWouldCross));
            return;
        }
        if r.tif == TimeInForce::Fok {
            let mut sim = self.clone();
            let (rem, cancelled) = sim.run_match(0, &r, &mut Vec::new());
            if cancelled || rem.0 != 0 {
                out.push(reject(RejectReason::FokUnfillable));
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        out.push(Event::Accepted {
            order_id: id,
            owner: r.owner,
            client_id: r.client_id,
            side: r.side,
            price: r.price,
            qty: r.qty,
        });
        let (rem, cancelled) = self.run_match(id, &r, out);
        if cancelled || rem.0 == 0 {
            return;
        }
        let mut o = RefOrder {
            id,
            owner: r.owner,
            client_id: r.client_id,
            side: r.side,
            price: r.price,
            remaining: rem,
            seq: 0,
            acct_seq: 0,
        };
        let reason = match r.tif {
            TimeInForce::Ioc | TimeInForce::Fok => Some(CancelReason::Unfilled),
            _ if self.orders.len() >= self.cfg.max_resting_orders as usize => {
                Some(CancelReason::BookFull)
            }
            _ if self.orders.iter().filter(|x| x.owner == r.owner).count()
                >= self.cfg.max_open_orders_per_account as usize =>
            {
                Some(CancelReason::AccountOrderLimit)
            }
            _ => None,
        };
        match reason {
            Some(reason) => out.push(Self::cancel_ev(&o, reason)),
            None => {
                self.seq += 1;
                o.seq = self.seq;
                o.acct_seq = self.seq;
                out.push(Event::Rested {
                    order_id: id,
                    owner: r.owner,
                    side: r.side,
                    price: r.price,
                    remaining: rem,
                });
                self.orders.push(o);
            }
        }
    }

    fn cancel(&mut self, owner: AccountId, id: OrderId, out: &mut Vec<Event>) -> bool {
        match self
            .orders
            .iter()
            .position(|o| o.id == id && o.owner == owner)
        {
            Some(i) => {
                let o = self.orders.remove(i);
                out.push(Self::cancel_ev(&o, CancelReason::UserRequested));
                true
            }
            None => false,
        }
    }

    fn cancel_all(&mut self, owner: AccountId, out: &mut Vec<Event>) -> u32 {
        // Engine emits newest-first (account list is LIFO).
        let mut mine: Vec<RefOrder> = self
            .orders
            .iter()
            .filter(|o| o.owner == owner)
            .cloned()
            .collect();
        mine.sort_by_key(|o| std::cmp::Reverse(o.acct_seq));
        for o in &mine {
            out.push(Self::cancel_ev(o, CancelReason::UserRequested));
        }
        self.orders.retain(|o| o.owner != owner);
        mine.len() as u32
    }

    fn snapshot(&self) -> Vec<OrderView> {
        let mut v: Vec<&RefOrder> = self.orders.iter().collect();
        v.sort_by(|a, b| {
            let side = |o: &RefOrder| o.side as u8;
            side(a)
                .cmp(&side(b))
                .then_with(|| match a.side {
                    Side::Bid => b.price.cmp(&a.price),
                    Side::Ask => a.price.cmp(&b.price),
                })
                .then(a.seq.cmp(&b.seq))
        });
        v.into_iter()
            .map(|o| OrderView {
                order_id: o.id,
                owner: o.owner,
                client_id: o.client_id,
                side: o.side,
                price: o.price,
                remaining: o.remaining,
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
enum Op {
    Place(OrderRequest),
    Cancel(AccountId, OrderId),
    CancelAll(AccountId),
}

fn arb_req() -> impl Strategy<Value = OrderRequest> {
    (
        1u64..=3,
        any::<u16>(),
        prop_oneof![Just(Side::Bid), Just(Side::Ask)],
        prop_oneof![9 => 95u64..=105, 1 => 0u64..=250],
        prop_oneof![9 => 1u64..=8, 1 => 0u64..=60],
        prop_oneof![
            4 => Just(TimeInForce::Gtc),
            2 => Just(TimeInForce::Ioc),
            1 => Just(TimeInForce::Fok),
            2 => Just(TimeInForce::PostOnly)
        ],
        prop_oneof![
            Just(StpMode::CancelTaker),
            Just(StpMode::CancelMaker),
            Just(StpMode::CancelBoth)
        ],
    )
        .prop_map(|(owner, cid, side, price, qty, tif, stp)| OrderRequest {
            owner,
            client_id: cid as u64,
            side,
            price: Price(price),
            qty: Qty(qty),
            tif,
            stp,
        })
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => arb_req().prop_map(Op::Place),
        4 => (1u64..=3, 1u64..=120).prop_map(|(o, id)| Op::Cancel(o, id)),
        1 => (1u64..=3).prop_map(Op::CancelAll),
    ]
}

fn snapshot(b: &OrderBook) -> Vec<OrderView> {
    let mut v = vec![];
    b.for_each_order(|o| v.push(*o));
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn engine_matches_reference(ops in prop::collection::vec(arb_op(), 1..120)) {
        let mut eng = OrderBook::new(cfg()).unwrap();
        let mut reference = RefBook::new(cfg());
        let (mut e_out, mut r_out) = (Vec::new(), Vec::new());

        for op in &ops {
            e_out.clear();
            r_out.clear();
            match *op {
                Op::Place(r) => {
                    eng.place(r, &mut e_out);
                    reference.place(r, &mut r_out);
                }
                Op::Cancel(owner, id) => {
                    let a = eng.cancel(owner, id, &mut e_out).is_ok();
                    let b = reference.cancel(owner, id, &mut r_out);
                    prop_assert_eq!(a, b);
                }
                Op::CancelAll(owner) => {
                    let a = eng.cancel_all(owner, &mut e_out);
                    let b = reference.cancel_all(owner, &mut r_out);
                    prop_assert_eq!(a, b);
                }
            }
            prop_assert_eq!(&e_out, &r_out, "op {:?}", op);
            if let Err(e) = eng.validate() {
                return Err(TestCaseError::fail(e));
            }
            prop_assert_eq!(snapshot(&eng), reference.snapshot());
        }
    }

    /// Quantity conservation: for every accepted order,
    /// qty == filled + resting remaining + cancelled remaining.
    #[test]
    fn quantity_is_conserved(ops in prop::collection::vec(arb_op(), 1..150)) {
        use std::collections::HashMap;
        let mut eng = OrderBook::new(cfg()).unwrap();
        let mut out = Vec::new();
        for op in &ops {
            match *op {
                Op::Place(r) => { eng.place(r, &mut out); }
                Op::Cancel(o, id) => { let _ = eng.cancel(o, id, &mut out); }
                Op::CancelAll(o) => { eng.cancel_all(o, &mut out); }
            }
        }
        let mut acc: HashMap<OrderId, (u64, u64)> = HashMap::new(); // (accepted, accounted)
        for e in &out {
            match *e {
                Event::Accepted { order_id, qty, .. } => { acc.entry(order_id).or_default().0 = qty.0; }
                Event::Fill(f) => {
                    acc.entry(f.maker_order).or_default().1 += f.qty.0;
                    acc.entry(f.taker_order).or_default().1 += f.qty.0;
                }
                Event::Cancelled { order_id, remaining, .. } => { acc.entry(order_id).or_default().1 += remaining.0; }
                _ => {}
            }
        }
        eng.for_each_order(|o| acc.entry(o.order_id).or_default().1 += o.remaining.0);
        for (id, (accepted, accounted)) in acc {
            prop_assert_eq!(accepted, accounted, "order {}", id);
        }
    }
}
