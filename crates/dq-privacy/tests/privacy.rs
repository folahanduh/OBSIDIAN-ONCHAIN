#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use dq_privacy::*;
use dq_types::Side;
use getrandom::rand_core::UnwrapErr;
use getrandom::SysRng;

const ALICE: u64 = 7;
const BOB: u64 = 8;
const DAY: u64 = EPOCH_MS;

fn rng() -> UnwrapErr<SysRng> {
    UnwrapErr(SysRng)
}

fn note(account: u64, ts_ms: u64, seq: u64) -> FillNote {
    FillNote {
        account,
        market: 3,
        side: Side::Bid,
        price: 100_250,
        qty: 40,
        fee: -1_234,
        fill_seq: seq,
        ts_ms,
        rseed: [seq as u8; 32],
    }
}

#[test]
fn note_roundtrip_and_encoding() {
    let seed = ViewingSeed::generate(&mut rng());
    let ts = 20_000 * DAY + 5;
    let k = seed.epoch_key(ALICE, epoch_of(ts));
    let n = note(ALICE, ts, 1);
    assert_eq!(FillNote::decode(&n.encode()).unwrap(), n);
    let enc = encrypt_note(&n, &k.public(), &mut rng()).unwrap();
    assert_eq!(enc.cm, n.commitment());
    assert_eq!(decrypt_note(&enc, &k).unwrap(), n);
}

#[test]
fn epoch_keys_are_independent_and_deterministic() {
    let seed = ViewingSeed::from_bytes([9; 32]);
    let a = seed.epoch_key(ALICE, 1).public();
    assert_eq!(a, seed.epoch_key(ALICE, 1).public());
    assert_ne!(a.pk, seed.epoch_key(ALICE, 2).public().pk);
    assert_ne!(a.pk, seed.epoch_key(BOB, 1).public().pk);
    assert!(a.is_valid());
}

#[test]
fn wrong_epoch_or_account_cannot_decrypt() {
    let seed = ViewingSeed::generate(&mut rng());
    let ts = 100 * DAY;
    let n = note(ALICE, ts, 1);
    let enc = encrypt_note(&n, &seed.epoch_key(ALICE, 100).public(), &mut rng()).unwrap();

    assert_eq!(
        decrypt_note(&enc, &seed.epoch_key(ALICE, 101)),
        Err(PrivacyError::WrongEpoch)
    );
    // Same epoch, different account key (or a forged epoch header) ⇒ AEAD failure.
    assert_eq!(
        decrypt_note(&enc, &seed.epoch_key(BOB, 100)),
        Err(PrivacyError::Decrypt)
    );
    let mut forged = enc.clone();
    forged.epoch = 101;
    assert_eq!(
        decrypt_note(&forged, &seed.epoch_key(ALICE, 101)),
        Err(PrivacyError::Decrypt)
    );

    // Sender-side guards.
    let pk = seed.epoch_key(ALICE, 100).public();
    assert_eq!(
        encrypt_note(&note(BOB, ts, 1), &pk, &mut rng()),
        Err(PrivacyError::WrongAccount)
    );
    assert_eq!(
        encrypt_note(&note(ALICE, ts + DAY, 1), &pk, &mut rng()),
        Err(PrivacyError::WrongEpoch)
    );
}

#[test]
fn any_tampering_is_detected() {
    let k = ViewingSeed::generate(&mut rng()).epoch_key(ALICE, 5);
    let enc = encrypt_note(&note(ALICE, 5 * DAY, 1), &k.public(), &mut rng()).unwrap();
    for i in 0..NOTE_LEN {
        let mut t = enc.clone();
        t.ct[i] ^= 1;
        assert!(decrypt_note(&t, &k).is_err());
    }
    let mut t = enc.clone();
    t.cm[0] ^= 1;
    assert_eq!(decrypt_note(&t, &k), Err(PrivacyError::Decrypt));
    let mut t = enc.clone();
    t.tag[15] ^= 1;
    assert_eq!(decrypt_note(&t, &k), Err(PrivacyError::Decrypt));
    let mut t = enc;
    t.epk[3] ^= 1;
    assert!(decrypt_note(&t, &k).is_err());
}

#[test]
fn commitments_hide_low_entropy_fields() {
    let mut a = note(ALICE, 0, 1);
    let mut b = a.clone();
    a.rseed = [1; 32];
    b.rseed = [2; 32];
    assert_ne!(a.commitment(), b.commitment());
}

#[test]
fn low_order_keys_rejected() {
    // Zero point and u=1 are small-order on Curve25519.
    for pk in [[0u8; 32], {
        let mut b = [0u8; 32];
        b[0] = 1;
        b
    }] {
        let bad = EpochViewingPublicKey {
            account: ALICE,
            epoch: 0,
            pk,
        };
        assert!(!bad.is_valid());
        assert_eq!(
            encrypt_note(&note(ALICE, 0, 1), &bad, &mut rng()),
            Err(PrivacyError::NonContributory)
        );
        assert_eq!(
            seal(
                SealPurpose::Order,
                b"x",
                &SequencerPublicKey(pk),
                &mut rng()
            ),
            Err(PrivacyError::NonContributory)
        );
    }
}

#[test]
fn compliance_disclosure_is_epoch_scoped() {
    let mut r = rng();
    let alice = ViewingSeed::generate(&mut r);
    let bob = ViewingSeed::generate(&mut r);
    let mut chain = vec![];
    for day in 10..20u64 {
        for (acct, seed) in [(ALICE, &alice), (BOB, &bob)] {
            let n = note(acct, day * DAY + 1_000, day);
            chain.push(encrypt_note(&n, &seed.epoch_key(acct, day).public(), &mut r).unwrap());
        }
    }
    // Alice discloses days 12..=14 to a CEX.
    let disc = ComplianceDisclosure::from_seed_range(&alice, ALICE, 12, 14).unwrap();
    let cex = SequencerKey::generate(&mut r);
    let sealed = seal(
        SealPurpose::Disclosure,
        &disc.encode(),
        &cex.public(),
        &mut r,
    )
    .unwrap();

    // CEX side.
    let received =
        ComplianceDisclosure::decode(&open(SealPurpose::Disclosure, &sealed, &cex).unwrap())
            .unwrap();
    assert_eq!(received.epochs().collect::<Vec<_>>(), vec![12, 13, 14]);
    let seen = received.scan(&chain);
    assert_eq!(
        seen.iter().map(|n| n.fill_seq).collect::<Vec<_>>(),
        vec![12, 13, 14]
    );
    assert!(seen.iter().all(|n| n.account == ALICE));

    // Mixed-account disclosures are refused.
    let mixed = vec![alice.epoch_key(ALICE, 1), bob.epoch_key(BOB, 2)];
    assert_eq!(
        ComplianceDisclosure::new(ALICE, mixed).unwrap_err(),
        PrivacyError::WrongAccount
    );
}

#[test]
fn seal_pads_and_authenticates() {
    let mut r = rng();
    let sk = SequencerKey::generate(&mut r);
    for len in [0usize, 1, 251, 252, 253, 1000] {
        let msg: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let env = seal(SealPurpose::Order, &msg, &sk.public(), &mut r).unwrap();
        assert_eq!(env.ct.len() % 256, 0);
        assert_eq!(env.ct.len(), (len + 4).div_ceil(256) * 256);
        let wire = SealedEnvelope::from_bytes(&env.to_bytes()).unwrap();
        assert_eq!(&*open(SealPurpose::Order, &wire, &sk).unwrap(), &msg[..]);
    }
    // Same-size bucket for 1..=252 byte payloads: no length leak within a bucket.
    let a = seal(SealPurpose::Order, &[0; 1], &sk.public(), &mut r).unwrap();
    let b = seal(SealPurpose::Order, &[0; 252], &sk.public(), &mut r).unwrap();
    assert_eq!(a.to_bytes().len(), b.to_bytes().len());

    // Cross-purpose and wrong-key opens fail.
    assert_eq!(
        open(SealPurpose::Disclosure, &a, &sk),
        Err(PrivacyError::Decrypt)
    );
    assert_eq!(
        open(SealPurpose::Order, &a, &SequencerKey::generate(&mut r)),
        Err(PrivacyError::Decrypt)
    );
    let mut t = a.clone();
    t.ct[10] ^= 0x80;
    assert_eq!(
        open(SealPurpose::Order, &t, &sk),
        Err(PrivacyError::Decrypt)
    );
    assert!(SealedEnvelope::from_bytes(&[0u8; 48 + 100]).is_err());
}

#[test]
fn ordering_log_detects_reordering() {
    let mut r = rng();
    let sk = SequencerKey::generate(&mut r);
    let envs: Vec<_> = (0..5u8)
        .map(|i| seal(SealPurpose::Order, &[i], &sk.public(), &mut r).unwrap())
        .collect();
    let mut log = OrderingLog::genesis(1);
    let receipts: Vec<_> = envs.iter().map(|e| log.append(e)).collect();
    assert!(receipts.iter().all(SequenceReceipt::verify));
    assert!(receipts.windows(2).all(|w| w[1].prev_head == w[0].head));
    assert_eq!(OrderingLog::replay(1, &envs), log);

    let mut swapped = envs.clone();
    swapped.swap(1, 2);
    assert_ne!(OrderingLog::replay(1, &swapped).head(), log.head());
    assert_ne!(
        OrderingLog::replay(2, &envs).head(),
        log.head(),
        "chain-id bound"
    );

    let mut forged = receipts[2];
    forged.leaf = OrderingLog::leaf(&envs[3]);
    assert!(!forged.verify());
}
