//! ABCI 2.0 application: translates CometBFT requests into `tenebra_chain::State` calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use sha2::{Digest, Sha256};
use tendermint_abci::Application;
use tendermint_proto::v0_38::abci::{
    ExecTxResult, RequestCheckTx, RequestFinalizeBlock, RequestInfo, RequestInitChain,
    RequestQuery, ResponseCheckTx, ResponseCommit, ResponseFinalizeBlock, ResponseInfo,
    ResponseInitChain, ResponseQuery, ValidatorUpdate as AbciValidatorUpdate,
};
use tendermint_proto::v0_38::crypto::{public_key, PublicKey};
use tendermint_proto::v0_38::types::BlockIdFlag;
use tenebra_chain::{
    Address, BlockHeader, Evidence, State, Tx, TxError, ValidatorUpdate, VoteInfo, NATIVE,
};

use crate::genesis_json::AppGenesis;
use crate::hex;

/// CometBFT validator address: first 20 bytes of SHA-256(ed25519 pubkey).
pub fn cometbft_address(pubkey: &[u8; 32]) -> [u8; 20] {
    let h = Sha256::digest(pubkey);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[..20]);
    a
}

#[derive(Default)]
struct Inner {
    state: Option<State>,
    app_hash: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct TenebraApp {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for TenebraApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TenebraApp")
    }
}

fn to_abci_update(u: &ValidatorUpdate) -> AbciValidatorUpdate {
    AbciValidatorUpdate {
        pub_key: Some(PublicKey {
            sum: Some(public_key::Sum::Ed25519(u.consensus_key.to_vec())),
        }),
        power: i64::try_from(u.power).unwrap_or(i64::MAX),
    }
}

/// Non-zero ABCI result code per error kind (0 = success).
fn error_code(e: &TxError) -> u32 {
    match e {
        TxError::Malformed => 1,
        TxError::WrongChain => 2,
        TxError::BadSignature => 3,
        TxError::BadNonce { .. } => 4,
        TxError::FeeCapBelowBaseFee => 5,
        TxError::BlockGasExceeded => 6,
        TxError::CannotPayFee => 7,
        TxError::InsufficientBalance => 8,
        TxError::ZeroAmount => 9,
        TxError::UnknownAsset => 10,
        TxError::UnknownValidator => 11,
        TxError::ValidatorExists => 12,
        TxError::InvalidConsensusKey => 13,
        TxError::ConsensusKeyInUse => 14,
        TxError::CommissionTooHigh => 15,
        TxError::SelfBondTooLow => 16,
        TxError::InsufficientDelegation => 17,
        TxError::ValidatorFullySlashed => 18,
        TxError::NothingToClaim => 19,
        TxError::NotJailed => 20,
        TxError::StillJailed => 21,
        TxError::Tombstoned => 22,
        TxError::Overflow => 23,
    }
}

impl TenebraApp {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a previous request panicked mid-update; the
        // state can no longer be trusted, so stop the node.
        self.inner.lock().unwrap_or_else(|_| std::process::exit(70))
    }

    fn query_json(state: &State, path: &str) -> Result<serde_json::Value, String> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        let addr = |s: &str| {
            hex::decode32(s)
                .map(Address)
                .ok_or_else(|| "bad address hex".to_string())
        };
        match parts.as_slice() {
            ["summary"] => {
                let c = state.counters();
                Ok(serde_json::json!({
                    "chain_id": state.params().chain_id,
                    "symbol": state.params().native_symbol,
                    "decimals": state.params().native_decimals,
                    "height": state.height(),
                    "time": state.time(),
                    "base_fee": state.base_fee().to_string(),
                    "supply": c.supply.to_string(),
                    "total_issued": c.total_issued.to_string(),
                    "total_fees_burned": c.total_fees_burned.to_string(),
                    "total_slashed": c.total_slashed.to_string(),
                    "issuance_rate_bps": state.issuance_rate_bps(state.time()),
                    "active_validators": state.active_set().iter().map(|(k, p)| serde_json::json!({
                        "consensus_key": hex::encode(k), "power": p
                    })).collect::<Vec<_>>(),
                }))
            }
            ["balance", a] => {
                Ok(serde_json::json!({ "balance": state.balance(addr(a)?, NATIVE).to_string() }))
            }
            ["balance", a, asset] => {
                let asset: u32 = asset.parse().map_err(|_| "bad asset id")?;
                Ok(serde_json::json!({ "balance": state.balance(addr(a)?, asset).to_string() }))
            }
            ["nonce", a] => Ok(serde_json::json!({ "nonce": state.nonce(addr(a)?) })),
            ["validator", a] => {
                let v = state.validator(addr(a)?).ok_or("unknown validator")?;
                Ok(serde_json::json!({
                    "operator": hex::encode(&v.operator.0),
                    "consensus_key": hex::encode(&v.consensus_key),
                    "tokens": v.tokens.to_string(),
                    "commission_bps": v.commission_bps,
                    "commission_owed": v.commission_owed.to_string(),
                    "jailed": v.jailed,
                    "tombstoned": v.tombstoned,
                }))
            }
            ["delegation", d, v] => {
                let (d, v) = (addr(d)?, addr(v)?);
                Ok(serde_json::json!({
                    "value": state.delegation_value(d, v).to_string(),
                    "pending_rewards": state.pending_rewards(d, v).to_string(),
                }))
            }
            _ => Err(format!("unknown query path {path:?}")),
        }
    }
}

impl Application for TenebraApp {
    fn info(&self, _req: RequestInfo) -> ResponseInfo {
        let g = self.lock();
        // State lives in memory: after a restart we report height 0 and
        // CometBFT replays the stored blocks through us (deterministic).
        ResponseInfo {
            data: "tenebra".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            app_version: 1,
            last_block_height: g
                .state
                .as_ref()
                .map_or(0, |s| i64::try_from(s.height()).unwrap_or(i64::MAX)),
            last_block_app_hash: g.app_hash.clone().into(),
        }
    }

    fn init_chain(&self, req: RequestInitChain) -> ResponseInitChain {
        let genesis_time = req.time.as_ref().map_or(0, |t| t.seconds);
        let genesis = match AppGenesis::parse(&req.app_state_bytes, genesis_time) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("invalid app_state in genesis.json: {e}");
                std::process::exit(78);
            }
        };
        let (state, updates) = match State::genesis(&genesis) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("genesis rejected: {e}");
                std::process::exit(78);
            }
        };
        let app_hash = state.app_hash().to_vec();
        let mut g = self.lock();
        g.app_hash = app_hash.clone();
        g.state = Some(state);
        ResponseInitChain {
            consensus_params: None,
            validators: updates.iter().map(to_abci_update).collect(),
            app_hash: app_hash.into(),
        }
    }

    fn check_tx(&self, req: RequestCheckTx) -> ResponseCheckTx {
        let g = self.lock();
        let res = match (Tx::decode(&req.tx), g.state.as_ref()) {
            (Ok(tx), Some(s)) => s.check_tx(&tx).map(|_| tx.kind.gas()),
            (Err(e), _) => Err(e),
            (_, None) => Err(TxError::Malformed),
        };
        match res {
            Ok(gas) => ResponseCheckTx {
                code: 0,
                gas_wanted: i64::try_from(gas).unwrap_or(i64::MAX),
                ..Default::default()
            },
            Err(e) => ResponseCheckTx {
                code: error_code(&e),
                log: format!("{e:?}"),
                ..Default::default()
            },
        }
    }

    fn finalize_block(&self, req: RequestFinalizeBlock) -> ResponseFinalizeBlock {
        let mut g = self.lock();
        let Some(state) = g.state.as_mut() else {
            eprintln!("finalize_block before init_chain");
            std::process::exit(70);
        };
        // Map CometBFT validator addresses back to consensus public keys.
        let by_addr: HashMap<[u8; 20], [u8; 32]> = state
            .validators()
            .map(|v| (cometbft_address(&v.consensus_key), v.consensus_key))
            .collect();
        let lookup = |a: &[u8]| {
            <[u8; 20]>::try_from(a)
                .ok()
                .and_then(|a| by_addr.get(&a).copied())
        };

        let last_votes = req
            .decided_last_commit
            .as_ref()
            .map(|c| {
                c.votes
                    .iter()
                    .filter_map(|v| {
                        let val = v.validator.as_ref()?;
                        Some(VoteInfo {
                            consensus_key: lookup(&val.address)?,
                            power: u64::try_from(val.power).unwrap_or(0),
                            // Commit and Nil votes both prove the validator is online.
                            signed: v.block_id_flag != BlockIdFlag::Absent as i32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let evidence = req
            .misbehavior
            .iter()
            .filter_map(|m| {
                Some(Evidence {
                    consensus_key: lookup(&m.validator.as_ref()?.address)?,
                    height: u64::try_from(m.height).ok()?,
                })
            })
            .collect();
        let header = BlockHeader {
            height: u64::try_from(req.height).unwrap_or(0),
            time: req.time.as_ref().map_or(state.time(), |t| t.seconds),
            last_votes,
            evidence,
        };

        // Undecodable txs get an error result; decodable ones go through the
        // state machine in block order.
        let decoded: Vec<Result<Tx, TxError>> = req.txs.iter().map(|b| Tx::decode(b)).collect();
        let valid: Vec<Tx> = decoded
            .iter()
            .filter_map(|r| r.as_ref().ok().cloned())
            .collect();
        let result = match state.finalize_block(&header, &valid) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cannot apply block {}: {e}", req.height);
                std::process::exit(70);
            }
        };
        let mut outcomes = result.txs.into_iter();
        let tx_results = decoded
            .iter()
            .map(|d| match d {
                Err(e) => ExecTxResult {
                    code: error_code(e),
                    log: format!("{e:?}"),
                    ..Default::default()
                },
                Ok(_) => {
                    let o = outcomes.next().unwrap_or(tenebra_chain::TxOutcome {
                        result: Err(TxError::Malformed),
                        gas_used: 0,
                        fee_burned: 0,
                    });
                    ExecTxResult {
                        code: o.result.as_ref().err().map_or(0, error_code),
                        log: match o.result {
                            Ok(()) => format!("ok; fee burned {}", o.fee_burned),
                            Err(e) => format!("{e:?}; fee burned {}", o.fee_burned),
                        },
                        gas_used: i64::try_from(o.gas_used).unwrap_or(i64::MAX),
                        ..Default::default()
                    }
                }
            })
            .collect();
        g.app_hash = result.app_hash.to_vec();
        ResponseFinalizeBlock {
            events: vec![],
            tx_results,
            validator_updates: result
                .validator_updates
                .iter()
                .map(to_abci_update)
                .collect(),
            consensus_param_updates: None,
            app_hash: result.app_hash.to_vec().into(),
        }
    }

    fn commit(&self) -> ResponseCommit {
        ResponseCommit { retain_height: 0 }
    }

    fn query(&self, req: RequestQuery) -> ResponseQuery {
        let g = self.lock();
        let Some(state) = g.state.as_ref() else {
            return ResponseQuery {
                code: 1,
                log: "chain not initialised".into(),
                ..Default::default()
            };
        };
        match Self::query_json(state, &req.path) {
            Ok(v) => ResponseQuery {
                code: 0,
                value: v.to_string().into_bytes().into(),
                height: i64::try_from(state.height()).unwrap_or(i64::MAX),
                ..Default::default()
            },
            Err(e) => ResponseQuery {
                code: 1,
                log: e,
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genesis_json::{AppGenesis, BalanceJson, ParamsJson, ValidatorJson};
    use ed25519_dalek::SigningKey;
    use tendermint_proto::google::protobuf::Timestamp;
    use tendermint_proto::v0_38::abci::{
        CommitInfo, Misbehavior, Validator as AbciValidator, VoteInfo as AbciVote,
    };
    use tenebra_chain::TxKind;

    const COIN: u128 = 1_000_000_000;

    #[test]
    fn address_matches_cometbft() {
        // From a priv_validator_key.json generated by `cometbft init` (v0.38.26).
        let pk = crate::hex::decode32(
            "06af9ae57506231db4ceaf0e6ef7ffd9e2d7a96f09bb0d4be04e3975620e2c32",
        )
        .unwrap();
        assert_eq!(
            crate::hex::encode(&cometbft_address(&pk)),
            "e3a5ad2d13d05a31b56e6c0826a649eef5f304ad"
        );
    }

    fn setup() -> (TenebraApp, SigningKey, [u8; 32]) {
        let alice = SigningKey::from_bytes(&[7; 32]);
        let op = SigningKey::from_bytes(&[8; 32]);
        let cons = SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes();
        let g = AppGenesis {
            params: ParamsJson::devnet(1, "TENEBERA"),
            assets: vec![],
            balances: vec![BalanceJson {
                address: crate::hex::encode(alice.verifying_key().as_bytes()),
                asset: 0,
                amount: (1_000 * COIN).to_string(),
            }],
            validators: vec![ValidatorJson {
                operator: crate::hex::encode(op.verifying_key().as_bytes()),
                consensus_key: crate::hex::encode(&cons),
                self_bond: (10_000 * COIN).to_string(),
                commission_bps: 500,
            }],
        };
        let app = TenebraApp::default();
        let res = app.init_chain(RequestInitChain {
            time: Some(Timestamp {
                seconds: 1_800_000_000,
                nanos: 0,
            }),
            app_state_bytes: serde_json::to_vec(&g).unwrap().into(),
            ..Default::default()
        });
        assert_eq!(res.validators.len(), 1);
        assert_eq!(res.validators[0].power, 10_000);
        assert_eq!(res.app_hash.len(), 32);
        (app, alice, cons)
    }

    fn block(
        app: &TenebraApp,
        height: i64,
        txs: Vec<Vec<u8>>,
        cons: [u8; 32],
        signed: bool,
        misbehave: bool,
    ) -> ResponseFinalizeBlock {
        let val = AbciValidator {
            address: cometbft_address(&cons).to_vec().into(),
            power: 10_000,
        };
        app.finalize_block(RequestFinalizeBlock {
            txs: txs.into_iter().map(Into::into).collect(),
            decided_last_commit: Some(CommitInfo {
                round: 0,
                votes: vec![AbciVote {
                    validator: Some(val.clone()),
                    block_id_flag: if signed {
                        BlockIdFlag::Commit
                    } else {
                        BlockIdFlag::Absent
                    } as i32,
                }],
            }),
            misbehavior: if misbehave {
                vec![Misbehavior {
                    validator: Some(val),
                    height: height - 1,
                    ..Default::default()
                }]
            } else {
                vec![]
            },
            height,
            time: Some(Timestamp {
                seconds: 1_800_000_000 + height,
                nanos: 0,
            }),
            ..Default::default()
        })
    }

    fn query(app: &TenebraApp, path: &str) -> serde_json::Value {
        let r = app.query(RequestQuery {
            path: path.into(),
            ..Default::default()
        });
        assert_eq!(r.code, 0, "{}", r.log);
        serde_json::from_slice(&r.value).unwrap()
    }

    #[test]
    fn check_and_finalize_a_transfer() {
        let (app, alice, cons) = setup();
        let bob = [3u8; 32];
        let tx = Tx::signed(
            1,
            &alice,
            0,
            10_000,
            TxKind::Transfer {
                to: Address(bob),
                asset: 0,
                amount: 5 * COIN,
            },
        );
        assert_eq!(
            app.check_tx(RequestCheckTx {
                tx: tx.encode().into(),
                ..Default::default()
            })
            .code,
            0
        );
        let bad = app.check_tx(RequestCheckTx {
            tx: vec![1, 2, 3].into(),
            ..Default::default()
        });
        assert_eq!(bad.code, error_code(&TxError::Malformed));

        let r = block(&app, 1, vec![tx.encode(), vec![0xff]], cons, true, false);
        assert_eq!(r.tx_results.len(), 2);
        assert_eq!(r.tx_results[0].code, 0, "{}", r.tx_results[0].log);
        assert_eq!(r.tx_results[1].code, error_code(&TxError::Malformed));
        assert_eq!(
            query(&app, &format!("balance/{}", crate::hex::encode(&bob)))["balance"],
            (5 * COIN).to_string()
        );
        assert_eq!(
            query(
                &app,
                &format!(
                    "nonce/{}",
                    crate::hex::encode(alice.verifying_key().as_bytes())
                )
            )["nonce"],
            1
        );
        let info = app.info(RequestInfo::default());
        assert_eq!(info.last_block_height, 1);
        assert_eq!(info.last_block_app_hash, r.app_hash);
    }

    #[test]
    fn votes_and_evidence_map_by_cometbft_address() {
        let (app, _, cons) = setup();
        block(&app, 1, vec![], cons, true, false);
        // Signed vote ⇒ issuance reached the validator (via address lookup).
        let s = query(&app, "summary");
        assert_ne!(s["total_issued"], "0");
        // Double-sign evidence ⇒ slashed and removed from the consensus set.
        let r = block(&app, 2, vec![], cons, true, true);
        assert_eq!(r.validator_updates.len(), 1);
        assert_eq!(r.validator_updates[0].power, 0);
        assert_ne!(query(&app, "summary")["total_slashed"], "0");
    }
}
