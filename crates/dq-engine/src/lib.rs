//! Deterministic price-time-priority central limit order book.
//!
//! Design constraints (this runs inside a replicated state machine):
//! - **Deterministic**: no floats, no clocks, no randomness, no iteration over
//!   hash maps. Same input sequence ⇒ same event stream ⇒ same book.
//! - **Bounded work per input**: every order touches at most
//!   `max_match_steps` resting orders, so a single transaction cannot stall the
//!   sequencer (sweep-the-book DoS).
//! - **O(1) cancel**: resting orders live in a slab with intrusive doubly-linked
//!   lists per price level *and* per account (for O(k) mass-cancel).
//! - **No unbounded market orders**: every order carries a limit price. A
//!   "market" order is an IOC with a protective limit computed client-side,
//!   which caps slippage and removes the classic thin-book sweep attack.
//!
//! Book keys: both sides are stored in ascending `BTreeMap<u64, Level>`. Asks
//! use `price` as key, bids use `!price` (`u64::MAX - price`), so the best level
//! of either side is always `first_entry()` and the cross test is a single
//! `key <= cross_key` comparison.

use std::collections::{BTreeMap, HashMap};

use dq_types::{AccountId, MarketSpec, OrderId, Price, Qty, Side};

const NIL: u32 = u32::MAX;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TimeInForce {
    /// Rest any unfilled remainder.
    Gtc,
    /// Cancel any unfilled remainder.
    Ioc,
    /// Fill completely in this step or reject without touching the book.
    Fok,
    /// Reject if it would take liquidity; otherwise rest.
    PostOnly,
}

/// Self-trade prevention. Applied when a taker would match a maker with the
/// same owner. Self-matching is never allowed: it is the primitive behind wash
/// trading and fee-rebate farming.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StpMode {
    CancelTaker,
    CancelMaker,
    CancelBoth,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    pub owner: AccountId,
    pub client_id: u64,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub tif: TimeInForce,
    pub stp: StpMode,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    ZeroQty,
    QtyBelowMin,
    QtyAboveMax,
    ZeroPrice,
    PriceAboveMax,
    PostOnlyWouldCross,
    FokUnfillable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    UserRequested,
    /// IOC/FOK remainder after matching.
    Unfilled,
    SelfTradeTaker,
    SelfTradeMaker,
    /// Taker hit `max_match_steps`; remainder cancelled (never rested, since it
    /// may still cross).
    MatchLimit,
    BookFull,
    AccountOrderLimit,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    /// Book-local strictly increasing fill sequence.
    pub seq: u64,
    pub maker_order: OrderId,
    pub maker_owner: AccountId,
    pub taker_order: OrderId,
    pub taker_owner: AccountId,
    pub taker_side: Side,
    /// Always the maker's resting price.
    pub price: Price,
    pub qty: Qty,
    pub maker_remaining: Qty,
    pub taker_remaining: Qty,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Accepted {
        order_id: OrderId,
        owner: AccountId,
        client_id: u64,
        side: Side,
        price: Price,
        qty: Qty,
    },
    Fill(Fill),
    Rested {
        order_id: OrderId,
        owner: AccountId,
        side: Side,
        price: Price,
        remaining: Qty,
    },
    Cancelled {
        order_id: OrderId,
        owner: AccountId,
        side: Side,
        price: Price,
        remaining: Qty,
        reason: CancelReason,
    },
    /// Pre-acceptance rejection; consumes no order id and does not mutate the book.
    Rejected {
        owner: AccountId,
        client_id: u64,
        reason: RejectReason,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelError {
    /// Unknown id *or* not owned by caller. Deliberately indistinguishable so
    /// cancel cannot be used as an oracle for other accounts' order ids.
    NotFound,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BookConfig {
    pub spec: MarketSpec,
    /// Global cap on resting orders (memory bound). Must be `< u32::MAX`.
    pub max_resting_orders: u32,
    pub max_open_orders_per_account: u32,
    /// Max resting orders a single taker may touch (fills + STP cancels).
    pub max_match_steps: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    Spec(dq_types::SpecError),
    ZeroLimit,
    CapacityTooLarge,
}

impl BookConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.spec.validate().map_err(ConfigError::Spec)?;
        if self.max_resting_orders == 0
            || self.max_open_orders_per_account == 0
            || self.max_match_steps == 0
        {
            return Err(ConfigError::ZeroLimit);
        }
        if self.max_resting_orders == NIL {
            return Err(ConfigError::CapacityTooLarge);
        }
        Ok(())
    }
}

/// Read-only view of a resting order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderView {
    pub order_id: OrderId,
    pub owner: AccountId,
    pub client_id: u64,
    pub side: Side,
    pub price: Price,
    pub remaining: Qty,
}

#[derive(Copy, Clone, Debug)]
struct Node {
    id: OrderId,
    owner: AccountId,
    client_id: u64,
    side: Side,
    price: Price,
    remaining: Qty,
    // price-level FIFO links
    prev: u32,
    next: u32,
    // per-account links
    aprev: u32,
    anext: u32,
}

impl Node {
    fn view(&self) -> OrderView {
        OrderView {
            order_id: self.id,
            owner: self.owner,
            client_id: self.client_id,
            side: self.side,
            price: self.price,
            remaining: self.remaining,
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct Level {
    head: u32,
    tail: u32,
    total: u128,
    count: u32,
}

impl Level {
    const EMPTY: Level = Level {
        head: NIL,
        tail: NIL,
        total: 0,
        count: 0,
    };
}

#[derive(Copy, Clone, Debug)]
struct AcctList {
    head: u32,
    count: u32,
}

/// Slab + id index + per-account intrusive lists.
#[derive(Debug, Default)]
struct OrderStore {
    nodes: Vec<Node>,
    free: Vec<u32>,
    index: HashMap<OrderId, u32>,
    accounts: HashMap<AccountId, AcctList>,
}

impl OrderStore {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn open_orders(&self, owner: AccountId) -> u32 {
        self.accounts.get(&owner).map_or(0, |a| a.count)
    }

    /// Allocate a slot, register in the id index and push onto the account list.
    fn alloc(&mut self, mut node: Node) -> u32 {
        let acct = self.accounts.entry(node.owner).or_insert(AcctList {
            head: NIL,
            count: 0,
        });
        node.prev = NIL;
        node.next = NIL;
        node.aprev = NIL;
        node.anext = acct.head;
        let idx = match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = node;
                i
            }
            None => {
                // Capacity is bounded by BookConfig::max_resting_orders < u32::MAX.
                let i = u32::try_from(self.nodes.len()).expect("slab bounded by config");
                self.nodes.push(node);
                i
            }
        };
        if acct.head != NIL {
            self.nodes[acct.head as usize].aprev = idx;
        }
        acct.head = idx;
        acct.count += 1;
        self.index.insert(node.id, idx);
        idx
    }

    /// Remove from id index and account list and free the slot. The caller
    /// must already have unlinked the node from its price level.
    fn release(&mut self, idx: u32) -> Node {
        let node = self.nodes[idx as usize];
        self.index.remove(&node.id);
        if node.aprev != NIL {
            self.nodes[node.aprev as usize].anext = node.anext;
        }
        if node.anext != NIL {
            self.nodes[node.anext as usize].aprev = node.aprev;
        }
        if let Some(acct) = self.accounts.get_mut(&node.owner) {
            if acct.head == idx {
                acct.head = node.anext;
            }
            acct.count -= 1;
            if acct.count == 0 {
                self.accounts.remove(&node.owner);
            }
        }
        self.free.push(idx);
        node
    }
}

fn level_push_back(level: &mut Level, nodes: &mut [Node], idx: u32) {
    let n = &mut nodes[idx as usize];
    n.prev = level.tail;
    n.next = NIL;
    let rem = n.remaining.0 as u128;
    if level.tail != NIL {
        nodes[level.tail as usize].next = idx;
    } else {
        level.head = idx;
    }
    level.tail = idx;
    level.total += rem;
    level.count += 1;
}

fn level_unlink(level: &mut Level, nodes: &mut [Node], idx: u32) {
    let n = nodes[idx as usize];
    if n.prev != NIL {
        nodes[n.prev as usize].next = n.next;
    } else {
        level.head = n.next;
    }
    if n.next != NIL {
        nodes[n.next as usize].prev = n.prev;
    } else {
        level.tail = n.prev;
    }
    level.total -= n.remaining.0 as u128;
    level.count -= 1;
}

#[inline]
fn side_key(side: Side, price: Price) -> u64 {
    match side {
        Side::Ask => price.0,
        Side::Bid => !price.0,
    }
}

#[inline]
fn key_price(side: Side, key: u64) -> Price {
    match side {
        Side::Ask => Price(key),
        Side::Bid => Price(!key),
    }
}

enum TakerOutcome {
    /// Matching stopped normally (filled, or no more crossing liquidity).
    Open,
    /// Taker was cancelled during matching (STP or match limit); event emitted.
    Cancelled,
}

#[derive(Debug)]
pub struct OrderBook {
    cfg: BookConfig,
    levels: [BTreeMap<u64, Level>; 2],
    store: OrderStore,
    next_order_id: OrderId,
    fill_seq: u64,
}

impl OrderBook {
    pub fn new(cfg: BookConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(OrderBook {
            cfg,
            levels: [BTreeMap::new(), BTreeMap::new()],
            store: OrderStore::default(),
            next_order_id: 1,
            fill_seq: 0,
        })
    }

    /// Pre-allocate slab, free list and id index for `max_resting_orders`.
    /// Call once at startup on production books: growth-triggered rehashing
    /// of the id index is a multi-millisecond tail-latency spike.
    pub fn reserve_capacity(&mut self) {
        let n = self.cfg.max_resting_orders as usize;
        self.store
            .nodes
            .reserve_exact(n.saturating_sub(self.store.nodes.len()));
        self.store.free.reserve_exact(n);
        self.store
            .index
            .reserve(n.saturating_sub(self.store.index.len()));
    }

    pub fn config(&self) -> &BookConfig {
        &self.cfg
    }

    fn validate_request(&self, req: &OrderRequest) -> Result<(), RejectReason> {
        let spec = &self.cfg.spec;
        if req.qty.0 == 0 {
            return Err(RejectReason::ZeroQty);
        }
        if req.qty < spec.min_qty {
            return Err(RejectReason::QtyBelowMin);
        }
        if req.qty > spec.max_qty {
            return Err(RejectReason::QtyAboveMax);
        }
        if req.price.0 == 0 {
            return Err(RejectReason::ZeroPrice);
        }
        if req.price > spec.max_price {
            return Err(RejectReason::PriceAboveMax);
        }
        Ok(())
    }

    /// Submit an order. Events are appended to `out` (caller reuses the buffer
    /// to keep the hot path allocation-free). Returns the assigned id if accepted.
    pub fn place(&mut self, req: OrderRequest, out: &mut Vec<Event>) -> Option<OrderId> {
        let reject = |reason| Event::Rejected {
            owner: req.owner,
            client_id: req.client_id,
            reason,
        };
        if let Err(reason) = self.validate_request(&req) {
            out.push(reject(reason));
            return None;
        }

        let opp = req.side.opposite();
        let cross_key = side_key(opp, req.price);
        let crosses = self.levels[opp.index()]
            .first_key_value()
            .is_some_and(|(k, _)| *k <= cross_key);

        match req.tif {
            TimeInForce::PostOnly if crosses => {
                out.push(reject(RejectReason::PostOnlyWouldCross));
                return None;
            }
            TimeInForce::Fok if !self.fok_fillable(&req) => {
                out.push(reject(RejectReason::FokUnfillable));
                return None;
            }
            _ => {}
        }

        let id = self.next_order_id;
        self.next_order_id += 1;
        out.push(Event::Accepted {
            order_id: id,
            owner: req.owner,
            client_id: req.client_id,
            side: req.side,
            price: req.price,
            qty: req.qty,
        });

        let mut remaining = req.qty;
        if crosses {
            if let TakerOutcome::Cancelled = self.match_taker(id, &req, &mut remaining, out) {
                return Some(id);
            }
        }
        if remaining.0 == 0 {
            return Some(id);
        }

        match req.tif {
            TimeInForce::Ioc | TimeInForce::Fok => out.push(Event::Cancelled {
                order_id: id,
                owner: req.owner,
                side: req.side,
                price: req.price,
                remaining,
                reason: CancelReason::Unfilled,
            }),
            TimeInForce::Gtc | TimeInForce::PostOnly => self.rest(id, &req, remaining, out),
        }
        Some(id)
    }

    fn match_taker(
        &mut self,
        taker_id: OrderId,
        req: &OrderRequest,
        remaining: &mut Qty,
        out: &mut Vec<Event>,
    ) -> TakerOutcome {
        let opp = req.side.opposite();
        let cross_key = side_key(opp, req.price);
        let book = &mut self.levels[opp.index()];
        let mut steps: u32 = 0;

        let cancel_taker = |remaining: Qty, reason| Event::Cancelled {
            order_id: taker_id,
            owner: req.owner,
            side: req.side,
            price: req.price,
            remaining,
            reason,
        };

        loop {
            if remaining.0 == 0 {
                return TakerOutcome::Open;
            }
            let Some(mut entry) = book.first_entry() else {
                return TakerOutcome::Open;
            };
            if *entry.key() > cross_key {
                return TakerOutcome::Open;
            }
            let level_price = key_price(opp, *entry.key());
            let level = entry.get_mut();
            let mut outcome = None;

            while remaining.0 > 0 && level.head != NIL {
                if steps == self.cfg.max_match_steps {
                    out.push(cancel_taker(*remaining, CancelReason::MatchLimit));
                    outcome = Some(TakerOutcome::Cancelled);
                    break;
                }
                steps += 1;

                let idx = level.head;
                let maker = self.store.nodes[idx as usize];

                if maker.owner == req.owner {
                    if req.stp == StpMode::CancelTaker {
                        out.push(cancel_taker(*remaining, CancelReason::SelfTradeTaker));
                        outcome = Some(TakerOutcome::Cancelled);
                        break;
                    }
                    level_unlink(level, &mut self.store.nodes, idx);
                    self.store.release(idx);
                    out.push(Event::Cancelled {
                        order_id: maker.id,
                        owner: maker.owner,
                        side: maker.side,
                        price: maker.price,
                        remaining: maker.remaining,
                        reason: CancelReason::SelfTradeMaker,
                    });
                    if req.stp == StpMode::CancelBoth {
                        out.push(cancel_taker(*remaining, CancelReason::SelfTradeTaker));
                        outcome = Some(TakerOutcome::Cancelled);
                        break;
                    }
                    continue;
                }

                let q = Qty(remaining.0.min(maker.remaining.0));
                remaining.0 -= q.0;
                let maker_remaining = Qty(maker.remaining.0 - q.0);
                self.store.nodes[idx as usize].remaining = maker_remaining;
                level.total -= q.0 as u128;
                self.fill_seq += 1;
                out.push(Event::Fill(Fill {
                    seq: self.fill_seq,
                    maker_order: maker.id,
                    maker_owner: maker.owner,
                    taker_order: taker_id,
                    taker_owner: req.owner,
                    taker_side: req.side,
                    price: level_price,
                    qty: q,
                    maker_remaining,
                    taker_remaining: *remaining,
                }));
                if maker_remaining.0 == 0 {
                    level_unlink(level, &mut self.store.nodes, idx);
                    self.store.release(idx);
                }
            }

            if level.head == NIL {
                entry.remove();
            }
            if let Some(o) = outcome {
                return o;
            }
        }
    }

    /// Non-mutating dry run mirroring `match_taker` step-for-step, so that
    /// `true` guarantees a complete fill.
    fn fok_fillable(&self, req: &OrderRequest) -> bool {
        let opp = req.side.opposite();
        let cross_key = side_key(opp, req.price);
        let mut need = req.qty.0;
        let mut steps: u32 = 0;
        for level in self.levels[opp.index()].range(..=cross_key).map(|(_, l)| l) {
            let mut idx = level.head;
            while idx != NIL {
                if steps == self.cfg.max_match_steps {
                    return false;
                }
                steps += 1;
                let n = &self.store.nodes[idx as usize];
                idx = n.next;
                if n.owner == req.owner {
                    if req.stp == StpMode::CancelMaker {
                        continue;
                    }
                    return false;
                }
                if n.remaining.0 >= need {
                    return true;
                }
                need -= n.remaining.0;
            }
        }
        false
    }

    fn rest(&mut self, id: OrderId, req: &OrderRequest, remaining: Qty, out: &mut Vec<Event>) {
        let reason = if self.store.len() >= self.cfg.max_resting_orders as usize {
            Some(CancelReason::BookFull)
        } else if self.store.open_orders(req.owner) >= self.cfg.max_open_orders_per_account {
            Some(CancelReason::AccountOrderLimit)
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push(Event::Cancelled {
                order_id: id,
                owner: req.owner,
                side: req.side,
                price: req.price,
                remaining,
                reason,
            });
            return;
        }
        let idx = self.store.alloc(Node {
            id,
            owner: req.owner,
            client_id: req.client_id,
            side: req.side,
            price: req.price,
            remaining,
            prev: NIL,
            next: NIL,
            aprev: NIL,
            anext: NIL,
        });
        let level = self.levels[req.side.index()]
            .entry(side_key(req.side, req.price))
            .or_insert(Level::EMPTY);
        level_push_back(level, &mut self.store.nodes, idx);
        out.push(Event::Rested {
            order_id: id,
            owner: req.owner,
            side: req.side,
            price: req.price,
            remaining,
        });
    }

    fn remove_resting(&mut self, idx: u32, reason: CancelReason, out: &mut Vec<Event>) {
        let n = self.store.nodes[idx as usize];
        let key = side_key(n.side, n.price);
        let book = &mut self.levels[n.side.index()];
        let level = book.get_mut(&key).expect("resting order has a level");
        level_unlink(level, &mut self.store.nodes, idx);
        if level.head == NIL {
            book.remove(&key);
        }
        self.store.release(idx);
        out.push(Event::Cancelled {
            order_id: n.id,
            owner: n.owner,
            side: n.side,
            price: n.price,
            remaining: n.remaining,
            reason,
        });
    }

    /// Cancel a resting order. Ownership is enforced here, not at the gateway.
    pub fn cancel(
        &mut self,
        owner: AccountId,
        order_id: OrderId,
        out: &mut Vec<Event>,
    ) -> Result<(), CancelError> {
        let idx = *self
            .store
            .index
            .get(&order_id)
            .ok_or(CancelError::NotFound)?;
        if self.store.nodes[idx as usize].owner != owner {
            return Err(CancelError::NotFound);
        }
        self.remove_resting(idx, CancelReason::UserRequested, out);
        Ok(())
    }

    /// Kill switch: cancel every resting order of `owner`. O(k) in the
    /// account's order count. Emission order is newest-first (deterministic).
    pub fn cancel_all(&mut self, owner: AccountId, out: &mut Vec<Event>) -> u32 {
        let mut n = 0;
        while let Some(head) = self.store.accounts.get(&owner).map(|a| a.head) {
            self.remove_resting(head, CancelReason::UserRequested, out);
            n += 1;
        }
        n
    }

    // ---------------------------------------------------------------- queries

    pub fn best(&self, side: Side) -> Option<(Price, u128)> {
        self.levels[side.index()]
            .first_key_value()
            .map(|(k, l)| (key_price(side, *k), l.total))
    }

    pub fn best_bid(&self) -> Option<(Price, u128)> {
        self.best(Side::Bid)
    }

    pub fn best_ask(&self) -> Option<(Price, u128)> {
        self.best(Side::Ask)
    }

    /// Aggregated top-`n` levels, best first.
    pub fn depth(&self, side: Side, n: usize) -> Vec<(Price, u128)> {
        self.levels[side.index()]
            .iter()
            .take(n)
            .map(|(k, l)| (key_price(side, *k), l.total))
            .collect()
    }

    pub fn order(&self, order_id: OrderId) -> Option<OrderView> {
        self.store
            .index
            .get(&order_id)
            .map(|&i| self.store.nodes[i as usize].view())
    }

    pub fn open_orders(&self, owner: AccountId) -> u32 {
        self.store.open_orders(owner)
    }

    pub fn resting_len(&self) -> usize {
        self.store.len()
    }

    pub fn next_order_id(&self) -> OrderId {
        self.next_order_id
    }

    /// Visit every resting order in canonical priority order: bids best→worst
    /// then asks best→worst, FIFO within a level. Use for state hashing.
    pub fn for_each_order(&self, mut f: impl FnMut(&OrderView)) {
        for side in [Side::Bid, Side::Ask] {
            for level in self.levels[side.index()].values() {
                let mut idx = level.head;
                while idx != NIL {
                    let n = &self.store.nodes[idx as usize];
                    f(&n.view());
                    idx = n.next;
                }
            }
        }
    }

    /// Full structural invariant check. O(n). Used by tests and as an
    /// optional post-block assertion in the sequencer.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = 0usize;
        for side in [Side::Bid, Side::Ask] {
            for (&key, level) in &self.levels[side.index()] {
                let price = key_price(side, key);
                if level.head == NIL || level.count == 0 {
                    return Err(format!("empty level {side:?}@{price}"));
                }
                let (mut idx, mut prev, mut total, mut count) = (level.head, NIL, 0u128, 0u32);
                while idx != NIL {
                    let n = &self.store.nodes[idx as usize];
                    if n.prev != prev {
                        return Err(format!("bad prev link at order {}", n.id));
                    }
                    if n.side != side || n.price != price {
                        return Err(format!("order {} on wrong level", n.id));
                    }
                    if n.remaining.0 == 0 {
                        return Err(format!("zero-qty resting order {}", n.id));
                    }
                    if self.store.index.get(&n.id) != Some(&idx) {
                        return Err(format!("index mismatch for order {}", n.id));
                    }
                    total += n.remaining.0 as u128;
                    count += 1;
                    prev = idx;
                    idx = n.next;
                }
                if prev != level.tail || total != level.total || count != level.count {
                    return Err(format!("level aggregate mismatch {side:?}@{price}"));
                }
                seen += count as usize;
            }
        }
        if seen != self.store.len() {
            return Err(format!(
                "index has {} entries, levels have {seen}",
                self.store.len()
            ));
        }
        let mut acct_total = 0usize;
        for (&owner, acct) in &self.store.accounts {
            let (mut idx, mut prev, mut count) = (acct.head, NIL, 0u32);
            while idx != NIL {
                let n = &self.store.nodes[idx as usize];
                if n.owner != owner || n.aprev != prev {
                    return Err(format!("account list corrupt for {owner}"));
                }
                count += 1;
                prev = idx;
                idx = n.anext;
            }
            if count != acct.count || count == 0 {
                return Err(format!("account count mismatch for {owner}"));
            }
            if count > self.cfg.max_open_orders_per_account {
                return Err(format!("account {owner} over order limit"));
            }
            acct_total += count as usize;
        }
        if acct_total != seen {
            return Err("account lists do not cover book".into());
        }
        if seen > self.cfg.max_resting_orders as usize {
            return Err("book over capacity".into());
        }
        if let (Some((bid, _)), Some((ask, _))) = (self.best_bid(), self.best_ask()) {
            if bid >= ask {
                return Err(format!("crossed book: bid {bid} >= ask {ask}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
