//! Session-key authorization.
//!
//! Two key classes per account, both Ed25519, never derived from each other:
//!
//! - **Master key** (cold / hardware wallet): signs [`SessionGrant`]s and
//!   [`MasterAction`]s (withdraw, revoke, viewing-key registration).
//! - **Session key** (hot, generated in the trading terminal, non-extractable
//!   WebCrypto key): signs [`SessionAction`]s only.
//!
//! The permission boundary is enforced by the type system first: a
//! [`SessionAction`] has no variant that moves funds, so no session key — no
//! matter how it is scoped or compromised — can withdraw. On top of that each
//! grant is scoped to a market allow-list, a per-order notional cap, a
//! validity window (≤ 7 days) and place/cancel flags.
//!
//! Replay protection:
//! - every signature is domain-separated by message type and bound to `chain_id`;
//! - session actions are bound to the grant's `grant_nonce`, and grant nonces
//!   are strictly increasing per account, so actions signed under an old grant
//!   can never be replayed under a re-issued one;
//! - per-session nonces use a 64-wide sliding window (RFC 4303 style), so
//!   parallel connections can land out of order without a global counter.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use tenebra_engine::{OrderRequest, StpMode, TimeInForce};
use tenebra_types::{AccountId, MarketId, MarketSpec, OrderId, Price, Qty, Side};

pub const DOMAIN_GRANT: &[u8] = b"Tenebra/v1/session-grant\0";
pub const DOMAIN_SESSION_ACTION: &[u8] = b"Tenebra/v1/session-action\0";
pub const DOMAIN_MASTER_ACTION: &[u8] = b"Tenebra/v1/master-action\0";

pub const MAX_GRANT_MARKETS: usize = 32;
pub const MAX_SESSION_TTL_MS: u64 = 7 * 86_400_000;
pub const MAX_VIEWING_KEYS_PER_TX: usize = 64;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    WrongChain,
    UnknownAccount,
    AccountExists,
    UnknownSession,
    DuplicateSession,
    TooManySessions,
    WeakKey,
    BadSignature,
    Replay,
    NotYetValid,
    Expired,
    BadGrant,
    StaleGrantNonce,
    PermissionDenied,
    Malformed,
}

// ------------------------------------------------------------ wire helpers

#[derive(Default)]
struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u128(&mut self, v: u128) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct R<'a>(&'a [u8]);
impl R<'_> {
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], AuthError> {
        if self.0.len() < N {
            return Err(AuthError::Malformed);
        }
        let (h, t) = self.0.split_at(N);
        self.0 = t;
        h.try_into().map_err(|_| AuthError::Malformed)
    }
    fn u8(&mut self) -> Result<u8, AuthError> {
        Ok(self.arr::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, AuthError> {
        self.arr().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, AuthError> {
        self.arr().map(u64::from_le_bytes)
    }
    fn end(&self) -> Result<(), AuthError> {
        self.0.is_empty().then_some(()).ok_or(AuthError::Malformed)
    }
}

fn tif_from(v: u8) -> Result<TimeInForce, AuthError> {
    Ok(match v {
        0 => TimeInForce::Gtc,
        1 => TimeInForce::Ioc,
        2 => TimeInForce::Fok,
        3 => TimeInForce::PostOnly,
        _ => return Err(AuthError::Malformed),
    })
}

fn tif_to(t: TimeInForce) -> u8 {
    match t {
        TimeInForce::Gtc => 0,
        TimeInForce::Ioc => 1,
        TimeInForce::Fok => 2,
        TimeInForce::PostOnly => 3,
    }
}

fn stp_from(v: u8) -> Result<StpMode, AuthError> {
    Ok(match v {
        0 => StpMode::CancelTaker,
        1 => StpMode::CancelMaker,
        2 => StpMode::CancelBoth,
        _ => return Err(AuthError::Malformed),
    })
}

fn stp_to(s: StpMode) -> u8 {
    match s {
        StpMode::CancelTaker => 0,
        StpMode::CancelMaker => 1,
        StpMode::CancelBoth => 2,
    }
}

fn parse_key(pk: &[u8; 32]) -> Result<VerifyingKey, AuthError> {
    let k = VerifyingKey::from_bytes(pk).map_err(|_| AuthError::WeakKey)?;
    if k.is_weak() {
        return Err(AuthError::WeakKey);
    }
    Ok(k)
}

fn verify(key: &VerifyingKey, msg: &[u8], sig: &[u8; 64]) -> Result<(), AuthError> {
    // verify_strict: rejects non-canonical S and small-order R/A (malleability).
    key.verify_strict(msg, &Signature::from_bytes(sig))
        .map_err(|_| AuthError::BadSignature)
}

// ------------------------------------------------------------ replay window

/// Sliding anti-replay window. Bit `i` of `bitmap` ⇔ nonce `max - i` seen.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    max: u64,
    bitmap: u64,
}

impl ReplayWindow {
    pub const WIDTH: u64 = 64;

    /// Would `n` be accepted? Non-mutating: call before signature verification.
    pub fn check(&self, n: u64) -> bool {
        if n == 0 {
            return false;
        }
        if n > self.max {
            return true;
        }
        let off = self.max - n;
        off < Self::WIDTH && self.bitmap & (1 << off) == 0
    }

    /// Record `n`. Only call after `check(n)` and successful verification.
    pub fn set(&mut self, n: u64) {
        if n > self.max {
            let shift = n - self.max;
            self.bitmap = if shift >= Self::WIDTH {
                0
            } else {
                self.bitmap << shift
            };
            self.bitmap |= 1;
            self.max = n;
        } else {
            self.bitmap |= 1 << (self.max - n);
        }
    }
}

// ------------------------------------------------------------ grants

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    /// Sorted, unique, non-empty, ≤ MAX_GRANT_MARKETS.
    pub markets: Vec<MarketId>,
    /// Per-order notional cap in quote atoms.
    pub max_order_notional: u128,
    pub can_place: bool,
    pub can_cancel: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGrant {
    pub chain_id: u64,
    pub account: AccountId,
    pub session_pk: [u8; 32],
    pub perms: Permissions,
    pub valid_from_ms: u64,
    pub expires_at_ms: u64,
    /// Strictly increasing per account.
    pub grant_nonce: u64,
}

impl SessionGrant {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.bytes(DOMAIN_GRANT)
            .u64(self.chain_id)
            .u64(self.account)
            .bytes(&self.session_pk)
            .u32(u32::try_from(self.perms.markets.len()).unwrap_or(u32::MAX));
        for m in &self.perms.markets {
            w.u32(*m);
        }
        w.u128(self.perms.max_order_notional)
            .u8(self.perms.can_place as u8)
            .u8(self.perms.can_cancel as u8)
            .u64(self.valid_from_ms)
            .u64(self.expires_at_ms)
            .u64(self.grant_nonce);
        w.0
    }

    pub fn sign(self, master: &SigningKey) -> SignedGrant {
        let sig = master.sign(&self.signing_bytes()).to_bytes();
        SignedGrant { grant: self, sig }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedGrant {
    pub grant: SessionGrant,
    pub sig: [u8; 64],
}

// ------------------------------------------------------------ session actions

/// Everything a session key may do. Deliberately has no fund-moving variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    Place {
        market: MarketId,
        client_id: u64,
        side: Side,
        price: Price,
        qty: Qty,
        tif: TimeInForce,
        stp: StpMode,
    },
    Cancel {
        market: MarketId,
        order_id: OrderId,
    },
    CancelAll {
        market: MarketId,
    },
}

impl SessionAction {
    pub fn market(&self) -> MarketId {
        match *self {
            SessionAction::Place { market, .. }
            | SessionAction::Cancel { market, .. }
            | SessionAction::CancelAll { market } => market,
        }
    }

    fn encode(&self, w: &mut W) {
        match *self {
            SessionAction::Place {
                market,
                client_id,
                side,
                price,
                qty,
                tif,
                stp,
            } => {
                w.u8(0)
                    .u32(market)
                    .u64(client_id)
                    .u8(side as u8)
                    .u64(price.0)
                    .u64(qty.0)
                    .u8(tif_to(tif))
                    .u8(stp_to(stp));
            }
            SessionAction::Cancel { market, order_id } => {
                w.u8(1).u32(market).u64(order_id);
            }
            SessionAction::CancelAll { market } => {
                w.u8(2).u32(market);
            }
        }
    }

    fn decode(r: &mut R<'_>) -> Result<Self, AuthError> {
        Ok(match r.u8()? {
            0 => SessionAction::Place {
                market: r.u32()?,
                client_id: r.u64()?,
                side: Side::from_u8(r.u8()?).ok_or(AuthError::Malformed)?,
                price: Price(r.u64()?),
                qty: Qty(r.u64()?),
                tif: tif_from(r.u8()?)?,
                stp: stp_from(r.u8()?)?,
            },
            1 => SessionAction::Cancel {
                market: r.u32()?,
                order_id: r.u64()?,
            },
            2 => SessionAction::CancelAll { market: r.u32()? },
            _ => return Err(AuthError::Malformed),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEnvelope {
    pub chain_id: u64,
    pub account: AccountId,
    pub session_pk: [u8; 32],
    pub grant_nonce: u64,
    pub nonce: u64,
    pub action: SessionAction,
    pub sig: [u8; 64],
}

impl ActionEnvelope {
    fn body(&self, w: &mut W) {
        w.u64(self.chain_id)
            .u64(self.account)
            .bytes(&self.session_pk)
            .u64(self.grant_nonce)
            .u64(self.nonce);
        self.action.encode(w);
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.bytes(DOMAIN_SESSION_ACTION);
        self.body(&mut w);
        w.0
    }

    pub fn new_signed(
        chain_id: u64,
        account: AccountId,
        grant_nonce: u64,
        nonce: u64,
        action: SessionAction,
        session: &SigningKey,
    ) -> Self {
        let mut env = ActionEnvelope {
            chain_id,
            account,
            session_pk: session.verifying_key().to_bytes(),
            grant_nonce,
            nonce,
            action,
            sig: [0; 64],
        };
        env.sig = session.sign(&env.signing_bytes()).to_bytes();
        env
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        self.body(&mut w);
        w.bytes(&self.sig);
        w.0
    }

    /// Canonical decode: exact length, valid tags, no trailing bytes.
    pub fn decode(b: &[u8]) -> Result<Self, AuthError> {
        let mut r = R(b);
        let env = ActionEnvelope {
            chain_id: r.u64()?,
            account: r.u64()?,
            session_pk: r.arr()?,
            grant_nonce: r.u64()?,
            nonce: r.u64()?,
            action: SessionAction::decode(&mut r)?,
            sig: r.arr()?,
        };
        r.end()?;
        Ok(env)
    }
}

// ------------------------------------------------------------ master actions

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MasterAction {
    /// Invalidate every session whose grant_nonce < `min_grant_nonce`.
    RevokeSessions { min_grant_nonce: u64 },
    Withdraw {
        asset: u32,
        amount: u128,
        destination: [u8; 32],
    },
    /// Register X25519 epoch viewing public keys for `first_epoch..`.
    RegisterViewingKeys {
        first_epoch: u64,
        pks: Vec<[u8; 32]>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterEnvelope {
    pub chain_id: u64,
    pub account: AccountId,
    pub nonce: u64,
    pub action: MasterAction,
    pub sig: [u8; 64],
}

impl MasterEnvelope {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.bytes(DOMAIN_MASTER_ACTION)
            .u64(self.chain_id)
            .u64(self.account)
            .u64(self.nonce);
        match &self.action {
            MasterAction::RevokeSessions { min_grant_nonce } => {
                w.u8(0).u64(*min_grant_nonce);
            }
            MasterAction::Withdraw {
                asset,
                amount,
                destination,
            } => {
                w.u8(1).u32(*asset).u128(*amount).bytes(destination);
            }
            MasterAction::RegisterViewingKeys { first_epoch, pks } => {
                w.u8(2)
                    .u64(*first_epoch)
                    .u32(u32::try_from(pks.len()).unwrap_or(u32::MAX));
                for pk in pks {
                    w.bytes(pk);
                }
            }
        }
        w.0
    }

    pub fn new_signed(
        chain_id: u64,
        account: AccountId,
        nonce: u64,
        action: MasterAction,
        master: &SigningKey,
    ) -> Self {
        let mut env = MasterEnvelope {
            chain_id,
            account,
            nonce,
            action,
            sig: [0; 64],
        };
        env.sig = master.sign(&env.signing_bytes()).to_bytes();
        env
    }
}

// ------------------------------------------------------------ authorizer

#[derive(Debug)]
struct Session {
    key: VerifyingKey,
    grant: SessionGrant,
    replay: ReplayWindow,
}

#[derive(Debug)]
struct AccountAuth {
    master: VerifyingKey,
    master_replay: ReplayWindow,
    last_grant_nonce: u64,
    min_grant_nonce: u64,
    sessions: HashMap<[u8; 32], Session>,
}

/// A session action that passed authentication and authorization. The owner
/// is taken from the authenticated account, never from client input.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Authorized {
    pub account: AccountId,
    pub action: SessionAction,
}

impl Authorized {
    pub fn order_request(&self) -> Option<(MarketId, OrderRequest)> {
        match self.action {
            SessionAction::Place {
                market,
                client_id,
                side,
                price,
                qty,
                tif,
                stp,
            } => Some((
                market,
                OrderRequest {
                    owner: self.account,
                    client_id,
                    side,
                    price,
                    qty,
                    tif,
                    stp,
                },
            )),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Authorizer {
    chain_id: u64,
    accounts: HashMap<AccountId, AccountAuth>,
    markets: HashMap<MarketId, MarketSpec>,
    max_sessions_per_account: usize,
}

impl Authorizer {
    pub fn new(
        chain_id: u64,
        markets: impl IntoIterator<Item = MarketSpec>,
        max_sessions_per_account: usize,
    ) -> Self {
        Authorizer {
            chain_id,
            accounts: HashMap::new(),
            markets: markets.into_iter().map(|m| (m.id, m)).collect(),
            max_sessions_per_account,
        }
    }

    pub fn register_account(
        &mut self,
        account: AccountId,
        master_pk: &[u8; 32],
    ) -> Result<(), AuthError> {
        if self.accounts.contains_key(&account) {
            return Err(AuthError::AccountExists);
        }
        let master = parse_key(master_pk)?;
        self.accounts.insert(
            account,
            AccountAuth {
                master,
                master_replay: ReplayWindow::default(),
                last_grant_nonce: 0,
                min_grant_nonce: 0,
                sessions: HashMap::new(),
            },
        );
        Ok(())
    }

    pub fn install_session(&mut self, sg: &SignedGrant, now_ms: u64) -> Result<(), AuthError> {
        let g = &sg.grant;
        if g.chain_id != self.chain_id {
            return Err(AuthError::WrongChain);
        }
        let acct = self
            .accounts
            .get_mut(&g.account)
            .ok_or(AuthError::UnknownAccount)?;
        verify(&acct.master, &g.signing_bytes(), &sg.sig)?;

        let key = parse_key(&g.session_pk)?;
        let p = &g.perms;
        let markets_ok = !p.markets.is_empty()
            && p.markets.len() <= MAX_GRANT_MARKETS
            && p.markets.windows(2).all(|w| w[0] < w[1])
            && p.markets.iter().all(|m| self.markets.contains_key(m));
        let window_ok = g.valid_from_ms < g.expires_at_ms
            && g.expires_at_ms - g.valid_from_ms <= MAX_SESSION_TTL_MS
            && now_ms < g.expires_at_ms;
        if !markets_ok || !window_ok || key == acct.master {
            return Err(AuthError::BadGrant);
        }
        if g.grant_nonce <= acct.last_grant_nonce || g.grant_nonce < acct.min_grant_nonce {
            return Err(AuthError::StaleGrantNonce);
        }
        acct.sessions.retain(|_, s| now_ms < s.grant.expires_at_ms);
        if acct.sessions.contains_key(&g.session_pk) {
            return Err(AuthError::DuplicateSession);
        }
        if acct.sessions.len() >= self.max_sessions_per_account {
            return Err(AuthError::TooManySessions);
        }
        acct.last_grant_nonce = g.grant_nonce;
        acct.sessions.insert(
            g.session_pk,
            Session {
                key,
                grant: g.clone(),
                replay: ReplayWindow::default(),
            },
        );
        Ok(())
    }

    pub fn authorize(
        &mut self,
        env: &ActionEnvelope,
        now_ms: u64,
    ) -> Result<Authorized, AuthError> {
        if env.chain_id != self.chain_id {
            return Err(AuthError::WrongChain);
        }
        let acct = self
            .accounts
            .get_mut(&env.account)
            .ok_or(AuthError::UnknownAccount)?;
        let s = acct
            .sessions
            .get_mut(&env.session_pk)
            .ok_or(AuthError::UnknownSession)?;
        let g = &s.grant;
        if g.grant_nonce != env.grant_nonce || g.grant_nonce < acct.min_grant_nonce {
            return Err(AuthError::UnknownSession);
        }
        if now_ms < g.valid_from_ms {
            return Err(AuthError::NotYetValid);
        }
        if now_ms >= g.expires_at_ms {
            return Err(AuthError::Expired);
        }
        // Cheap rejects first; the nonce is only consumed by a valid signature,
        // so an attacker cannot burn a victim's nonce space.
        if !s.replay.check(env.nonce) {
            return Err(AuthError::Replay);
        }
        verify(&s.key, &env.signing_bytes(), &env.sig)?;
        s.replay.set(env.nonce);

        let p = &g.perms;
        let market = env.action.market();
        if p.markets.binary_search(&market).is_err() {
            return Err(AuthError::PermissionDenied);
        }
        let allowed = match env.action {
            SessionAction::Place { price, qty, .. } => {
                let spec = self
                    .markets
                    .get(&market)
                    .ok_or(AuthError::PermissionDenied)?;
                let notional = (price.0 as u128)
                    .checked_mul(qty.0 as u128)
                    .and_then(|x| x.checked_mul(spec.tick_size as u128));
                p.can_place && notional.is_some_and(|n| n <= p.max_order_notional)
            }
            SessionAction::Cancel { .. } | SessionAction::CancelAll { .. } => p.can_cancel,
        };
        if !allowed {
            return Err(AuthError::PermissionDenied);
        }
        Ok(Authorized {
            account: env.account,
            action: env.action,
        })
    }

    /// Verify a master-signed action. `RevokeSessions` is applied here; other
    /// actions are returned for the ledger/registry to execute.
    pub fn authorize_master(&mut self, env: &MasterEnvelope) -> Result<MasterAction, AuthError> {
        if env.chain_id != self.chain_id {
            return Err(AuthError::WrongChain);
        }
        let acct = self
            .accounts
            .get_mut(&env.account)
            .ok_or(AuthError::UnknownAccount)?;
        if !acct.master_replay.check(env.nonce) {
            return Err(AuthError::Replay);
        }
        if let MasterAction::RegisterViewingKeys { pks, .. } = &env.action {
            if pks.is_empty() || pks.len() > MAX_VIEWING_KEYS_PER_TX {
                return Err(AuthError::Malformed);
            }
        }
        verify(&acct.master, &env.signing_bytes(), &env.sig)?;
        acct.master_replay.set(env.nonce);
        if let MasterAction::RevokeSessions { min_grant_nonce } = env.action {
            acct.min_grant_nonce = acct.min_grant_nonce.max(min_grant_nonce);
            let min = acct.min_grant_nonce;
            acct.sessions.retain(|_, s| s.grant.grant_nonce >= min);
        }
        Ok(env.action.clone())
    }

    pub fn session_count(&self, account: AccountId) -> usize {
        self.accounts.get(&account).map_or(0, |a| a.sessions.len())
    }
}

#[cfg(test)]
mod tests;
