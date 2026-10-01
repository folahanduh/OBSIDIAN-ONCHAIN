use super::*;

const CHAIN: u64 = 0xDA2C;
const ALICE: AccountId = 1;
const NOW: u64 = 1_800_000_000_000;

fn key(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
}

fn spec(id: MarketId) -> MarketSpec {
    MarketSpec {
        id,
        tick_size: 10,
        lot_size: 1,
        min_qty: Qty(1),
        max_qty: Qty(1 << 32),
        max_price: Price(1 << 32),
    }
}

fn grant(session: &SigningKey, nonce: u64) -> SessionGrant {
    SessionGrant {
        chain_id: CHAIN,
        account: ALICE,
        session_pk: session.verifying_key().to_bytes(),
        perms: Permissions {
            markets: vec![1, 2],
            max_order_notional: 1_000_000,
            can_place: true,
            can_cancel: true,
        },
        valid_from_ms: NOW - 1,
        expires_at_ms: NOW + 3_600_000,
        grant_nonce: nonce,
    }
}

fn place(market: MarketId, price: u64, qty: u64) -> SessionAction {
    SessionAction::Place {
        market,
        client_id: 42,
        side: Side::Bid,
        price: Price(price),
        qty: Qty(qty),
        tif: TimeInForce::Gtc,
        stp: StpMode::CancelTaker,
    }
}

/// Authorizer with ALICE registered (master = key(1)) and session key(2) installed.
fn setup() -> (Authorizer, SigningKey, SigningKey) {
    let (master, session) = (key(1), key(2));
    let mut a = Authorizer::new(CHAIN, [spec(1), spec(2), spec(3)], 4);
    a.register_account(ALICE, &master.verifying_key().to_bytes())
        .unwrap();
    a.install_session(&grant(&session, 1).sign(&master), NOW)
        .unwrap();
    (a, master, session)
}

fn env(
    session: &SigningKey,
    grant_nonce: u64,
    nonce: u64,
    action: SessionAction,
) -> ActionEnvelope {
    ActionEnvelope::new_signed(CHAIN, ALICE, grant_nonce, nonce, action, session)
}

#[test]
fn happy_path_injects_owner() {
    let (mut a, _, s) = setup();
    let ok = a
        .authorize(&env(&s, 1, 1, place(1, 100, 1_000)), NOW)
        .unwrap();
    let (market, req) = ok.order_request().unwrap();
    assert_eq!((market, req.owner, req.qty), (1, ALICE, Qty(1_000)));
}

#[test]
fn scope_is_enforced() {
    let (mut a, _, s) = setup();
    // notional = 100 * 1001 * 10 = 1_001_000 > cap
    assert_eq!(
        a.authorize(&env(&s, 1, 1, place(1, 100, 1_001)), NOW),
        Err(AuthError::PermissionDenied)
    );
    assert_eq!(
        a.authorize(&env(&s, 1, 2, place(3, 1, 1)), NOW),
        Err(AuthError::PermissionDenied)
    );
    // Overflowing notional is denied, not wrapped.
    assert_eq!(
        a.authorize(&env(&s, 1, 3, place(1, u64::MAX, u64::MAX)), NOW),
        Err(AuthError::PermissionDenied)
    );
}

#[test]
fn place_and_cancel_flags() {
    let (master, s) = (key(1), key(3));
    let mut a = Authorizer::new(CHAIN, [spec(1)], 4);
    a.register_account(ALICE, &master.verifying_key().to_bytes())
        .unwrap();
    let mut g = grant(&s, 1);
    g.perms.markets = vec![1];
    g.perms.can_place = false;
    a.install_session(&g.sign(&master), NOW).unwrap();
    assert_eq!(
        a.authorize(&env(&s, 1, 1, place(1, 1, 1)), NOW),
        Err(AuthError::PermissionDenied)
    );
    assert!(a
        .authorize(&env(&s, 1, 2, SessionAction::CancelAll { market: 1 }), NOW)
        .is_ok());
}

#[test]
fn replay_window_semantics() {
    let (mut a, _, s) = setup();
    let e = |n| {
        env(
            &s,
            1,
            n,
            SessionAction::Cancel {
                market: 1,
                order_id: 9,
            },
        )
    };
    assert!(a.authorize(&e(100), NOW).is_ok());
    assert_eq!(a.authorize(&e(100), NOW), Err(AuthError::Replay));
    assert!(
        a.authorize(&e(90), NOW).is_ok(),
        "out-of-order within window"
    );
    assert_eq!(a.authorize(&e(90), NOW), Err(AuthError::Replay));
    assert_eq!(
        a.authorize(&e(36), NOW),
        Err(AuthError::Replay),
        "older than window"
    );
    assert!(a.authorize(&e(37), NOW).is_ok(), "oldest slot in window");
    assert!(a.authorize(&e(1_000), NOW).is_ok());
    assert_eq!(a.authorize(&e(990), NOW).map(|_| ()), Ok(()));
    assert_eq!(
        a.authorize(&e(0), NOW),
        Err(AuthError::Replay),
        "0 reserved"
    );
}

#[test]
fn denied_action_still_consumes_nonce() {
    let (mut a, _, s) = setup();
    let e = env(&s, 1, 5, place(3, 1, 1));
    assert_eq!(a.authorize(&e, NOW), Err(AuthError::PermissionDenied));
    assert_eq!(a.authorize(&e, NOW), Err(AuthError::Replay));
}

#[test]
fn bad_signatures_do_not_consume_nonce() {
    let (mut a, master, s) = setup();
    let mut forged = env(&master, 1, 7, place(1, 1, 1));
    forged.session_pk = s.verifying_key().to_bytes();
    assert_eq!(a.authorize(&forged, NOW), Err(AuthError::BadSignature));
    let mut tampered = env(&s, 1, 7, place(1, 1, 1));
    tampered.action = place(1, 2, 1);
    assert_eq!(a.authorize(&tampered, NOW), Err(AuthError::BadSignature));
    assert!(a.authorize(&env(&s, 1, 7, place(1, 1, 1)), NOW).is_ok());
}

#[test]
fn time_window_and_chain_binding() {
    let (mut a, _, s) = setup();
    let e = env(&s, 1, 1, place(1, 1, 1));
    assert_eq!(a.authorize(&e, NOW - 2), Err(AuthError::NotYetValid));
    assert_eq!(a.authorize(&e, NOW + 3_600_000), Err(AuthError::Expired));
    let other = ActionEnvelope::new_signed(CHAIN + 1, ALICE, 1, 2, place(1, 1, 1), &s);
    assert_eq!(a.authorize(&other, NOW), Err(AuthError::WrongChain));
}

#[test]
fn grant_validation() {
    let (mut a, master, _) = setup();
    let s = key(9);
    // Signed by the wrong key.
    assert_eq!(
        a.install_session(&grant(&s, 2).sign(&s), NOW),
        Err(AuthError::BadSignature)
    );
    // Non-monotonic grant nonce.
    assert_eq!(
        a.install_session(&grant(&s, 1).sign(&master), NOW),
        Err(AuthError::StaleGrantNonce)
    );
    // TTL too long.
    let mut g = grant(&s, 2);
    g.expires_at_ms = g.valid_from_ms + MAX_SESSION_TTL_MS + 1;
    assert_eq!(
        a.install_session(&g.sign(&master), NOW),
        Err(AuthError::BadGrant)
    );
    // Unsorted / unknown markets.
    let mut g = grant(&s, 2);
    g.perms.markets = vec![2, 1];
    assert_eq!(
        a.install_session(&g.sign(&master), NOW),
        Err(AuthError::BadGrant)
    );
    let mut g = grant(&s, 2);
    g.perms.markets = vec![99];
    assert_eq!(
        a.install_session(&g.sign(&master), NOW),
        Err(AuthError::BadGrant)
    );
    // Session key may not equal the master key.
    assert_eq!(
        a.install_session(&grant(&master, 2).sign(&master), NOW),
        Err(AuthError::BadGrant)
    );
    // Tampering with the grant after signing.
    let mut sg = grant(&s, 2).sign(&master);
    sg.grant.perms.max_order_notional = u128::MAX;
    assert_eq!(a.install_session(&sg, NOW), Err(AuthError::BadSignature));
    assert!(a.install_session(&grant(&s, 2).sign(&master), NOW).is_ok());
}

#[test]
fn weak_session_key_rejected() {
    let (mut a, master, _) = setup();
    let mut g = grant(&key(9), 2);
    // Identity point (small order): y = 1.
    g.session_pk = [0; 32];
    g.session_pk[0] = 1;
    assert_eq!(
        a.install_session(&g.sign(&master), NOW),
        Err(AuthError::WeakKey)
    );
}

#[test]
fn revoke_then_reissue_blocks_cross_grant_replay() {
    let (mut a, master, s) = setup();
    let old = env(&s, 1, 1, place(1, 1, 1));
    assert!(a.authorize(&old, NOW).is_ok());

    let revoke = MasterEnvelope::new_signed(
        CHAIN,
        ALICE,
        1,
        MasterAction::RevokeSessions { min_grant_nonce: 2 },
        &master,
    );
    a.authorize_master(&revoke).unwrap();
    assert_eq!(a.session_count(ALICE), 0);
    assert_eq!(
        a.authorize(&env(&s, 1, 2, place(1, 1, 1)), NOW),
        Err(AuthError::UnknownSession)
    );

    // Re-issue a grant for the *same* session key: fresh replay window, but
    // old envelopes are bound to grant_nonce 1 and stay dead.
    a.install_session(&grant(&s, 2).sign(&master), NOW).unwrap();
    assert_eq!(a.authorize(&old, NOW), Err(AuthError::UnknownSession));
    assert!(a.authorize(&env(&s, 2, 1, place(1, 1, 1)), NOW).is_ok());
    assert_eq!(a.authorize_master(&revoke), Err(AuthError::Replay));
}

#[test]
fn session_key_cannot_sign_master_actions() {
    let (mut a, master, s) = setup();
    let w = MasterAction::Withdraw {
        asset: 0,
        amount: 1,
        destination: [7; 32],
    };
    let forged = MasterEnvelope::new_signed(CHAIN, ALICE, 1, w.clone(), &s);
    assert_eq!(a.authorize_master(&forged), Err(AuthError::BadSignature));
    let ok = MasterEnvelope::new_signed(CHAIN, ALICE, 1, w.clone(), &master);
    assert_eq!(a.authorize_master(&ok), Ok(w));
}

#[test]
fn cross_type_signature_confusion_rejected() {
    // A grant signature must not verify as a master action and vice versa:
    // all signing payloads carry distinct domain prefixes.
    let (_, master, s) = setup();
    let g = grant(&s, 5).signing_bytes();
    let m = MasterEnvelope::new_signed(
        CHAIN,
        ALICE,
        1,
        MasterAction::RevokeSessions { min_grant_nonce: 0 },
        &master,
    )
    .signing_bytes();
    let e = env(&s, 1, 1, place(1, 1, 1)).signing_bytes();
    assert!(g.starts_with(DOMAIN_GRANT));
    assert!(m.starts_with(DOMAIN_MASTER_ACTION));
    assert!(e.starts_with(DOMAIN_SESSION_ACTION));
}

#[test]
fn session_cap() {
    let (mut a, master, _) = setup();
    for i in 2..=4u8 {
        a.install_session(&grant(&key(10 + i), i as u64).sign(&master), NOW)
            .unwrap();
    }
    assert_eq!(
        a.install_session(&grant(&key(20), 5).sign(&master), NOW),
        Err(AuthError::TooManySessions)
    );
    // Expired sessions are pruned on install.
    let later = NOW + 3_600_000;
    let mut g = grant(&key(20), 6);
    g.valid_from_ms = later;
    g.expires_at_ms = later + 1_000;
    assert!(a.install_session(&g.sign(&master), later).is_ok());
    assert_eq!(a.session_count(ALICE), 1);
}

#[test]
fn wire_roundtrip_is_canonical() {
    let s = key(2);
    for action in [
        place(1, 123, 456),
        SessionAction::Cancel {
            market: 2,
            order_id: 77,
        },
        SessionAction::CancelAll { market: 1 },
    ] {
        let e = env(&s, 1, 9, action);
        let b = e.encode();
        assert_eq!(ActionEnvelope::decode(&b).unwrap(), e);
        let mut trailing = b.clone();
        trailing.push(0);
        assert_eq!(ActionEnvelope::decode(&trailing), Err(AuthError::Malformed));
        assert_eq!(
            ActionEnvelope::decode(&b[..b.len() - 1]),
            Err(AuthError::Malformed)
        );
    }
    let mut bad_tag = env(&s, 1, 9, place(1, 1, 1)).encode();
    bad_tag[8 + 8 + 32 + 8 + 8] = 9; // action tag
    assert_eq!(ActionEnvelope::decode(&bad_tag), Err(AuthError::Malformed));
}

#[test]
fn replay_window_unit() {
    let mut w = ReplayWindow::default();
    for n in [5, 3, 4, 1, 2] {
        assert!(w.check(n));
        w.set(n);
        assert!(!w.check(n));
    }
    w.set(5 + 200);
    assert!(!w.check(5 + 200 - 64));
    assert!(w.check(5 + 200 - 63));
}
