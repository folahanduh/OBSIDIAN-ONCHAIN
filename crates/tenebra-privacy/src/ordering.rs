//! Commit-before-decrypt ordering log.
//!
//! `head₀ = H_chain("genesis" ‖ chain_id)`,
//! `leafₙ = H_leaf(sealed envelope bytes)`,
//! `headₙ = H_chain(headₙ₋₁ ‖ n ‖ leafₙ)`.
//!
//! The sequencer appends ciphertexts in arrival order, signs and publishes
//! `(n, headₙ)` and returns a [`SequenceReceipt`] to the client — all *before*
//! it decrypts the batch. Reordering on content would require two signed
//! receipts for the same height with different leaves, which is a slashable,
//! publicly verifiable equivocation.

use crate::seal::SealedEnvelope;
use crate::{CTX_SEQ_CHAIN, CTX_SEQ_LEAF};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SequenceReceipt {
    pub height: u64,
    pub leaf: [u8; 32],
    pub prev_head: [u8; 32],
    pub head: [u8; 32],
}

impl SequenceReceipt {
    pub fn verify(&self) -> bool {
        chain_step(&self.prev_head, self.height, &self.leaf) == self.head
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderingLog {
    height: u64,
    head: [u8; 32],
}

fn chain_step(prev: &[u8; 32], height: u64, leaf: &[u8; 32]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key(CTX_SEQ_CHAIN)
        .update(prev)
        .update(&height.to_le_bytes())
        .update(leaf)
        .finalize()
        .as_bytes()
}

impl OrderingLog {
    pub fn genesis(chain_id: u64) -> Self {
        let head = *blake3::Hasher::new_derive_key(CTX_SEQ_CHAIN)
            .update(b"genesis")
            .update(&chain_id.to_le_bytes())
            .finalize()
            .as_bytes();
        OrderingLog { height: 0, head }
    }

    pub fn leaf(env: &SealedEnvelope) -> [u8; 32] {
        *blake3::Hasher::new_derive_key(CTX_SEQ_LEAF)
            .update(&env.to_bytes())
            .finalize()
            .as_bytes()
    }

    pub fn append(&mut self, env: &SealedEnvelope) -> SequenceReceipt {
        let leaf = Self::leaf(env);
        let prev_head = self.head;
        self.height += 1;
        self.head = chain_step(&prev_head, self.height, &leaf);
        SequenceReceipt {
            height: self.height,
            leaf,
            prev_head,
            head: self.head,
        }
    }

    pub fn head(&self) -> [u8; 32] {
        self.head
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    /// Recompute the head from genesis over a sequence of envelopes.
    pub fn replay<'a>(chain_id: u64, envs: impl IntoIterator<Item = &'a SealedEnvelope>) -> Self {
        let mut log = Self::genesis(chain_id);
        for e in envs {
            log.append(e);
        }
        log
    }
}
