use super::*;
use dq_types::MarketSpec;

fn cfg() -> BookConfig {
    BookConfig {
        spec: MarketSpec {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            min_qty: Qty(1),
            max_qty: Qty(1_000_000),
            max_price: Price(1_000_000),
        },
        max_resting_orders: 1_000,
        max_open_orders_per_account: 100,
        max_match_steps: 64,
    }
}

fn book() -> OrderBook {
    OrderBook::new(cfg()).unwrap()
}

fn req(owner: AccountId, side: Side, price: u64, qty: u64, tif: TimeInForce) -> OrderRequest {
    OrderRequest {
        owner,
        client_id: 0,
        side,
        price: Price(price),
        qty: Qty(qty),
        tif,
        stp: StpMode::CancelTaker,
    }
}

fn place(b: &mut OrderBook, r: OrderRequest) -> (Option<OrderId>, Vec<Event>) {
    let mut out = Vec::new();
    let id = b.place(r, &mut out);
    b.validate().unwrap();
    (id, out)
}

fn fills(ev: &[Event]) -> Vec<(OrderId, u64, u64)> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Fill(f) => Some((f.maker_order, f.price.0, f.qty.0)),
            _ => None,
        })
        .collect()
}

fn cancels(ev: &[Event]) -> Vec<(OrderId, CancelReason)> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Cancelled {
                order_id, reason, ..
            } => Some((*order_id, *reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn rests_and_reports_top_of_book() {
    let mut b = book();
    place(&mut b, req(1, Side::Bid, 99, 5, TimeInForce::Gtc));
    place(&mut b, req(1, Side::Bid, 100, 3, TimeInForce::Gtc));
    place(&mut b, req(2, Side::Ask, 102, 4, TimeInForce::Gtc));
    assert_eq!(b.best_bid(), Some((Price(100), 3)));
    assert_eq!(b.best_ask(), Some((Price(102), 4)));
    assert_eq!(
        b.depth(Side::Bid, 10),
        vec![(Price(100), 3), (Price(99), 5)]
    );
}

#[test]
fn price_time_priority_and_maker_price() {
    let mut b = book();
    let (a1, _) = place(&mut b, req(1, Side::Ask, 101, 2, TimeInForce::Gtc));
    let (a2, _) = place(&mut b, req(2, Side::Ask, 101, 2, TimeInForce::Gtc));
    let (a3, _) = place(&mut b, req(3, Side::Ask, 100, 1, TimeInForce::Gtc));
    // Aggressive bid at 105 trades at maker prices, best price first then FIFO.
    let (_, ev) = place(&mut b, req(9, Side::Bid, 105, 4, TimeInForce::Ioc));
    assert_eq!(
        fills(&ev),
        vec![
            (a3.unwrap(), 100, 1),
            (a1.unwrap(), 101, 2),
            (a2.unwrap(), 101, 1)
        ]
    );
    assert_eq!(b.order(a2.unwrap()).unwrap().remaining, Qty(1));
    assert_eq!(b.best_ask(), Some((Price(101), 1)));
}

#[test]
fn gtc_remainder_rests_after_sweep() {
    let mut b = book();
    place(&mut b, req(1, Side::Ask, 100, 2, TimeInForce::Gtc));
    let (id, ev) = place(&mut b, req(2, Side::Bid, 101, 5, TimeInForce::Gtc));
    assert!(matches!(
        ev.last(),
        Some(Event::Rested {
            remaining: Qty(3),
            ..
        })
    ));
    assert_eq!(b.best_bid(), Some((Price(101), 3)));
    assert_eq!(b.best_ask(), None);
    assert_eq!(b.order(id.unwrap()).unwrap().remaining, Qty(3));
}

#[test]
fn ioc_remainder_cancelled() {
    let mut b = book();
    place(&mut b, req(1, Side::Ask, 100, 2, TimeInForce::Gtc));
    let (id, ev) = place(&mut b, req(2, Side::Bid, 100, 5, TimeInForce::Ioc));
    assert_eq!(cancels(&ev), vec![(id.unwrap(), CancelReason::Unfilled)]);
    assert_eq!(b.resting_len(), 0);
}

#[test]
fn fok_all_or_nothing() {
    let mut b = book();
    place(&mut b, req(1, Side::Ask, 100, 2, TimeInForce::Gtc));
    place(&mut b, req(1, Side::Ask, 101, 2, TimeInForce::Gtc));
    let next = b.next_order_id();

    let (id, ev) = place(&mut b, req(2, Side::Bid, 101, 5, TimeInForce::Fok));
    assert_eq!(id, None);
    assert!(matches!(
        ev[..],
        [Event::Rejected {
            reason: RejectReason::FokUnfillable,
            ..
        }]
    ));
    assert_eq!(b.next_order_id(), next, "rejection must not consume an id");
    assert_eq!(b.resting_len(), 2);

    let (_, ev) = place(&mut b, req(2, Side::Bid, 101, 4, TimeInForce::Fok));
    assert_eq!(fills(&ev).len(), 2);
    assert_eq!(b.resting_len(), 0);
}

#[test]
fn post_only_rejects_when_crossing() {
    let mut b = book();
    place(&mut b, req(1, Side::Ask, 100, 2, TimeInForce::Gtc));
    let (id, ev) = place(&mut b, req(2, Side::Bid, 100, 1, TimeInForce::PostOnly));
    assert_eq!(id, None);
    assert!(matches!(
        ev[..],
        [Event::Rejected {
            reason: RejectReason::PostOnlyWouldCross,
            ..
        }]
    ));
    let (id, _) = place(&mut b, req(2, Side::Bid, 99, 1, TimeInForce::PostOnly));
    assert!(id.is_some());
}

#[test]
fn stp_cancel_taker() {
    let mut b = book();
    place(&mut b, req(2, Side::Ask, 100, 1, TimeInForce::Gtc));
    let (own, _) = place(&mut b, req(1, Side::Ask, 100, 1, TimeInForce::Gtc));
    let (tid, ev) = place(&mut b, req(1, Side::Bid, 100, 5, TimeInForce::Gtc));
    assert_eq!(fills(&ev).len(), 1, "fills ahead of own order");
    assert_eq!(
        cancels(&ev),
        vec![(tid.unwrap(), CancelReason::SelfTradeTaker)]
    );
    assert!(b.order(own.unwrap()).is_some(), "maker untouched");
}

#[test]
fn stp_cancel_maker_continues_matching() {
    let mut b = book();
    let (own, _) = place(&mut b, req(1, Side::Ask, 100, 1, TimeInForce::Gtc));
    let (other, _) = place(&mut b, req(2, Side::Ask, 100, 1, TimeInForce::Gtc));
    let mut r = req(1, Side::Bid, 100, 1, TimeInForce::Ioc);
    r.stp = StpMode::CancelMaker;
    let (_, ev) = place(&mut b, r);
    assert_eq!(
        cancels(&ev),
        vec![(own.unwrap(), CancelReason::SelfTradeMaker)]
    );
    assert_eq!(fills(&ev), vec![(other.unwrap(), 100, 1)]);
}

#[test]
fn stp_cancel_both() {
    let mut b = book();
    let (own, _) = place(&mut b, req(1, Side::Ask, 100, 1, TimeInForce::Gtc));
    let mut r = req(1, Side::Bid, 100, 1, TimeInForce::Gtc);
    r.stp = StpMode::CancelBoth;
    let (tid, ev) = place(&mut b, r);
    assert_eq!(
        cancels(&ev),
        vec![
            (own.unwrap(), CancelReason::SelfTradeMaker),
            (tid.unwrap(), CancelReason::SelfTradeTaker)
        ]
    );
    assert_eq!(b.resting_len(), 0);
}

#[test]
fn cancel_enforces_ownership_without_oracle() {
    let mut b = book();
    let (id, _) = place(&mut b, req(1, Side::Bid, 100, 1, TimeInForce::Gtc));
    let mut out = Vec::new();
    assert_eq!(
        b.cancel(2, id.unwrap(), &mut out),
        Err(CancelError::NotFound)
    );
    assert_eq!(b.cancel(2, 999, &mut out), Err(CancelError::NotFound));
    assert!(out.is_empty());
    assert_eq!(b.cancel(1, id.unwrap(), &mut out), Ok(()));
    assert_eq!(b.resting_len(), 0);
    b.validate().unwrap();
}

#[test]
fn cancel_middle_of_level_keeps_fifo() {
    let mut b = book();
    let ids: Vec<_> = (0..3)
        .map(|i| {
            place(&mut b, req(i + 1, Side::Bid, 100, 1, TimeInForce::Gtc))
                .0
                .unwrap()
        })
        .collect();
    let mut out = Vec::new();
    b.cancel(2, ids[1], &mut out).unwrap();
    b.validate().unwrap();
    let (_, ev) = place(&mut b, req(9, Side::Ask, 100, 2, TimeInForce::Ioc));
    assert_eq!(fills(&ev), vec![(ids[0], 100, 1), (ids[2], 100, 1)]);
}

#[test]
fn cancel_all_kill_switch() {
    let mut b = book();
    for p in 90..95 {
        place(&mut b, req(1, Side::Bid, p, 1, TimeInForce::Gtc));
    }
    place(&mut b, req(2, Side::Bid, 95, 1, TimeInForce::Gtc));
    let mut out = Vec::new();
    assert_eq!(b.cancel_all(1, &mut out), 5);
    b.validate().unwrap();
    assert_eq!(b.open_orders(1), 0);
    assert_eq!(b.resting_len(), 1);
}

#[test]
fn match_step_limit_bounds_work() {
    let mut b = OrderBook::new(BookConfig {
        max_match_steps: 3,
        ..cfg()
    })
    .unwrap();
    for i in 0..5 {
        place(&mut b, req(10 + i, Side::Ask, 100, 1, TimeInForce::Gtc));
    }
    let (tid, ev) = place(&mut b, req(1, Side::Bid, 100, 5, TimeInForce::Gtc));
    assert_eq!(fills(&ev).len(), 3);
    assert_eq!(cancels(&ev), vec![(tid.unwrap(), CancelReason::MatchLimit)]);
    assert_eq!(b.best_bid(), None, "remainder must not rest");

    // FOK precheck honours the same limit.
    let (id, _) = place(&mut b, req(1, Side::Bid, 100, 2, TimeInForce::Fok));
    assert!(id.is_some());
    let (id, _) = place(&mut b, req(1, Side::Bid, 100, 1, TimeInForce::Fok));
    assert!(id.is_none(), "book empty now");
}

#[test]
fn capacity_limits() {
    let mut b = OrderBook::new(BookConfig {
        max_resting_orders: 2,
        max_open_orders_per_account: 1,
        ..cfg()
    })
    .unwrap();
    place(&mut b, req(1, Side::Bid, 100, 1, TimeInForce::Gtc));
    let (id, ev) = place(&mut b, req(1, Side::Bid, 99, 1, TimeInForce::Gtc));
    assert_eq!(
        cancels(&ev),
        vec![(id.unwrap(), CancelReason::AccountOrderLimit)]
    );
    place(&mut b, req(2, Side::Bid, 98, 1, TimeInForce::Gtc));
    let (id, ev) = place(&mut b, req(3, Side::Bid, 97, 1, TimeInForce::Gtc));
    assert_eq!(cancels(&ev), vec![(id.unwrap(), CancelReason::BookFull)]);
}

#[test]
fn input_validation() {
    let mut b = book();
    let cases = [
        (
            req(1, Side::Bid, 100, 0, TimeInForce::Gtc),
            RejectReason::ZeroQty,
        ),
        (
            req(1, Side::Bid, 0, 1, TimeInForce::Gtc),
            RejectReason::ZeroPrice,
        ),
        (
            req(1, Side::Bid, 2_000_000, 1, TimeInForce::Gtc),
            RejectReason::PriceAboveMax,
        ),
        (
            req(1, Side::Bid, 1, 2_000_000, TimeInForce::Gtc),
            RejectReason::QtyAboveMax,
        ),
    ];
    for (r, want) in cases {
        let (id, ev) = place(&mut b, r);
        assert_eq!(id, None);
        assert!(matches!(ev[..], [Event::Rejected { reason, .. }] if reason == want));
    }
}

#[test]
fn slab_slots_are_reused() {
    let mut b = book();
    for _ in 0..100 {
        let (id, _) = place(&mut b, req(1, Side::Bid, 100, 1, TimeInForce::Gtc));
        b.cancel(1, id.unwrap(), &mut Vec::new()).unwrap();
    }
    assert!(b.store.nodes.len() <= 1);
}
