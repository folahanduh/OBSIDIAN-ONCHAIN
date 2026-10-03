//! Tenebra L1 state machine.
//!
//! The native coin pays gas for every transaction (the fee is burned),
//! secures consensus through validator staking, and is slashed for provable
//! misbehaviour. Consensus itself (networking, block voting, finality) is
//! CometBFT; this crate is the deterministic application it drives via ABCI.
//! See `docs/L1.md`.

#![forbid(unsafe_code)]

pub mod journal;
pub mod state;
pub mod tx;
pub mod types;

pub use state::*;
pub use tx::{gas, Tx, TxKind};
pub use types::*;
