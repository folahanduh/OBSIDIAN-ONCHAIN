//! Transaction-shape enforcement via the instructions sysvar.
//!
//! A guarded swap transaction must contain, contiguously:
//!
//! ```text
//! [k]   tenebra_guard::pre_swap        (top-level, not CPI)
//! [k+1] exactly one allowed router ix  (ComputeBudget ixs may be interleaved)
//! [k+2] tenebra_guard::post_swap       (referencing the same session account)
//! ```
//!
//! Setup instructions (ATA creation, SOL wrapping) go before `pre_swap` and
//! cleanup (SOL unwrapping) after `post_swap`. Because `pre_swap` refuses to
//! run unless the matching `post_swap` is present later in the same
//! transaction, fee collection and the slippage check cannot be stripped off.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked,
};

use crate::error::GuardError;

/// ComputeBudget111111111111111111111111111111
pub const COMPUTE_BUDGET_ID: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");

/// Index of the session account in `post_swap`'s account list (asserted by test).
pub const POST_SWAP_SESSION_INDEX: usize = 1;

/// Minimal view of an instruction, so the rules are unit-testable without a VM.
#[derive(Clone, Debug)]
pub struct IxView {
    pub program_id: Pubkey,
    pub data: Vec<u8>,
    pub accounts: Vec<Pubkey>,
}

fn is_ix(ix: &IxView, program: &Pubkey, disc: &[u8]) -> bool {
    ix.program_id == *program && ix.data.len() >= disc.len() && &ix.data[..disc.len()] == disc
}

/// Pure layout rule. `current` must be this program's `pre_swap`.
pub fn check_layout(
    ixs: &[IxView],
    current: usize,
    program_id: &Pubkey,
    pre_disc: &[u8],
    post_disc: &[u8],
    routers: &[Pubkey],
    session: &Pubkey,
) -> core::result::Result<(), GuardError> {
    let me = ixs.get(current).ok_or(GuardError::NotTopLevel)?;
    if !is_ix(me, program_id, pre_disc) {
        // We were invoked via CPI (or the sysvar is lying): refuse.
        return Err(GuardError::NotTopLevel);
    }
    let mut router_seen = false;
    for ix in &ixs[current + 1..] {
        if ix.program_id == *program_id {
            if !is_ix(ix, program_id, post_disc) || !router_seen {
                return Err(GuardError::BadLayout);
            }
            return match ix.accounts.get(POST_SWAP_SESSION_INDEX) {
                Some(s) if s == session => Ok(()),
                _ => Err(GuardError::SessionMismatch),
            };
        }
        if ix.program_id == COMPUTE_BUDGET_ID {
            continue;
        }
        if !routers.contains(&ix.program_id) {
            return Err(GuardError::RouterNotAllowed);
        }
        if router_seen {
            return Err(GuardError::BadLayout);
        }
        router_seen = true;
    }
    Err(GuardError::BadLayout)
}

/// Load every instruction from the sysvar and return (views, current index).
pub fn load_all(sysvar: &AccountInfo) -> Result<(Vec<IxView>, usize)> {
    let current = load_current_index_checked(sysvar)? as usize;
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Ok(ix) = load_instruction_at_checked(i, sysvar) {
        out.push(IxView {
            program_id: ix.program_id,
            data: ix.data,
            accounts: ix.accounts.iter().map(|m| m.pubkey).collect(),
        });
        i += 1;
    }
    Ok((out, current))
}

/// True if instruction `current` is a top-level call of `disc` on this program.
pub fn is_top_level(sysvar: &AccountInfo, program_id: &Pubkey, disc: &[u8]) -> Result<bool> {
    let current = load_current_index_checked(sysvar)? as usize;
    let ix = load_instruction_at_checked(current, sysvar)?;
    Ok(ix.program_id == *program_id
        && ix.data.len() >= disc.len()
        && &ix.data[..disc.len()] == disc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRE: &[u8] = &[1; 8];
    const POST: &[u8] = &[2; 8];

    fn ids() -> (Pubkey, Pubkey, Pubkey, Pubkey) {
        (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }

    fn ix(p: Pubkey, d: &[u8], accts: Vec<Pubkey>) -> IxView {
        IxView {
            program_id: p,
            data: d.to_vec(),
            accounts: accts,
        }
    }

    fn run(
        ixs: &[IxView],
        cur: usize,
        me: Pubkey,
        router: Pubkey,
        session: Pubkey,
    ) -> core::result::Result<(), GuardError> {
        check_layout(ixs, cur, &me, PRE, POST, &[router], &session)
    }

    #[test]
    fn accepts_canonical_layout_with_setup_and_cleanup() {
        let (me, router, session, other) = ids();
        let user = Pubkey::new_unique();
        let txn = [
            ix(COMPUTE_BUDGET_ID, &[0], vec![]),
            ix(other, &[9], vec![]), // ATA create before pre is fine
            ix(me, PRE, vec![]),
            ix(router, &[7], vec![]),
            ix(COMPUTE_BUDGET_ID, &[0], vec![]),
            ix(me, POST, vec![user, session]),
            ix(other, &[9], vec![]), // unwrap after post is fine
        ];
        assert_eq!(run(&txn, 2, me, router, session), Ok(()));
    }

    #[test]
    fn rejects_missing_post() {
        let (me, router, session, _) = ids();
        let txn = [ix(me, PRE, vec![]), ix(router, &[7], vec![])];
        assert_eq!(
            run(&txn, 0, me, router, session),
            Err(GuardError::BadLayout)
        );
    }

    #[test]
    fn rejects_unlisted_program_between() {
        let (me, router, session, other) = ids();
        let txn = [
            ix(me, PRE, vec![]),
            ix(other, &[7], vec![]),
            ix(me, POST, vec![Pubkey::default(), session]),
        ];
        assert_eq!(
            run(&txn, 0, me, router, session),
            Err(GuardError::RouterNotAllowed)
        );
    }

    #[test]
    fn rejects_zero_or_two_router_ixs() {
        let (me, router, session, _) = ids();
        let post = ix(me, POST, vec![Pubkey::default(), session]);
        let none = [ix(me, PRE, vec![]), post.clone()];
        assert_eq!(
            run(&none, 0, me, router, session),
            Err(GuardError::BadLayout)
        );
        let two = [
            ix(me, PRE, vec![]),
            ix(router, &[7], vec![]),
            ix(router, &[7], vec![]),
            post,
        ];
        assert_eq!(
            run(&two, 0, me, router, session),
            Err(GuardError::BadLayout)
        );
    }

    #[test]
    fn rejects_wrong_session_or_nested_pre() {
        let (me, router, session, _) = ids();
        let wrong = [
            ix(me, PRE, vec![]),
            ix(router, &[7], vec![]),
            ix(me, POST, vec![Pubkey::default(), Pubkey::new_unique()]),
        ];
        assert_eq!(
            run(&wrong, 0, me, router, session),
            Err(GuardError::SessionMismatch)
        );
        let nested = [
            ix(me, PRE, vec![]),
            ix(router, &[7], vec![]),
            ix(me, PRE, vec![]),
        ];
        assert_eq!(
            run(&nested, 0, me, router, session),
            Err(GuardError::BadLayout)
        );
    }

    #[test]
    fn rejects_cpi_invocation() {
        let (me, router, session, other) = ids();
        // Current top-level ix belongs to another program: we were CPI'd.
        let txn = [
            ix(other, PRE, vec![]),
            ix(router, &[7], vec![]),
            ix(me, POST, vec![Pubkey::default(), session]),
        ];
        assert_eq!(
            run(&txn, 0, me, router, session),
            Err(GuardError::NotTopLevel)
        );
    }
}
