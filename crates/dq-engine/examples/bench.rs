//! Single-thread matching latency benchmark.
//!
//! `cargo run -p dq-engine --release --example bench`
//!
//! Workload: book pre-seeded around mid; then a mixed stream of
//! 50% passive GTC, 37% cancels, 13% aggressive IOC (1–3 levels deep).

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::time::Instant;

use dq_engine::*;
use dq_types::{MarketSpec, OrderId, Price, Qty, Side};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const MID: u64 = 1_000_000;
const OPS: usize = 2_000_000;

fn main() {
    let mut book = OrderBook::new(BookConfig {
        spec: MarketSpec {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            min_qty: Qty(1),
            max_qty: Qty(1 << 30),
            max_price: Price(1 << 40),
        },
        max_resting_orders: 1 << 20,
        max_open_orders_per_account: 1 << 20,
        max_match_steps: 64,
    })
    .unwrap();
    book.reserve_capacity();
    let mut rng = Rng(0x0BAD_5EED);
    let mut out = Vec::with_capacity(256);
    let mut live: Vec<(u64, OrderId)> = Vec::with_capacity(1 << 20);

    let passive = |rng: &mut Rng| {
        let side = if rng.below(2) == 0 {
            Side::Bid
        } else {
            Side::Ask
        };
        let off = 1 + rng.below(200);
        let price = match side {
            Side::Bid => MID - off,
            Side::Ask => MID + off,
        };
        let owner = rng.below(1_000);
        (
            owner,
            OrderRequest {
                owner,
                client_id: 0,
                side,
                price: Price(price),
                qty: Qty(1 + rng.below(100)),
                tif: TimeInForce::Gtc,
                stp: StpMode::CancelMaker,
            },
        )
    };

    for _ in 0..20_000 {
        let (owner, r) = passive(&mut rng);
        out.clear();
        if let Some(id) = book.place(r, &mut out) {
            live.push((owner, id));
        }
    }

    let mut lat = Vec::with_capacity(OPS);
    let t_all = Instant::now();
    for _ in 0..OPS {
        out.clear();
        let roll = rng.below(100);
        let t = Instant::now();
        if roll < 50 {
            let (owner, r) = passive(&mut rng);
            if let Some(id) = book.place(r, &mut out) {
                live.push((owner, id));
            }
        } else if roll < 87 && !live.is_empty() {
            let i = rng.below(live.len() as u64) as usize;
            let (owner, id) = live.swap_remove(i);
            let _ = black_box(book.cancel(owner, id, &mut out));
        } else {
            let side = if rng.below(2) == 0 {
                Side::Bid
            } else {
                Side::Ask
            };
            let px = match side {
                Side::Bid => MID + 1 + rng.below(3),
                Side::Ask => MID - 1 - rng.below(3),
            };
            book.place(
                OrderRequest {
                    owner: 5_000 + rng.below(100),
                    client_id: 0,
                    side,
                    price: Price(px),
                    qty: Qty(1 + rng.below(300)),
                    tif: TimeInForce::Ioc,
                    stp: StpMode::CancelTaker,
                },
                &mut out,
            );
        }
        lat.push(t.elapsed().as_nanos() as u64);
        black_box(&out);
    }
    let total = t_all.elapsed();
    book.validate().unwrap();

    lat.sort_unstable();
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    println!("ops            {OPS}");
    println!("resting (end)  {}", book.resting_len());
    println!(
        "throughput     {:.2} M ops/s",
        OPS as f64 / total.as_secs_f64() / 1e6
    );
    println!(
        "latency ns     p50 {}  p90 {}  p99 {}  p99.9 {}  max {}",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(0.999),
        lat[lat.len() - 1]
    );
}
