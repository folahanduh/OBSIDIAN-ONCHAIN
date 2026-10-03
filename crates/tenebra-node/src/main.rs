//! `tenebra-node` — run the Tenebra L1 application under CometBFT, set up a
//! devnet, and send transactions / queries over CometBFT's RPC.
//!
//! Devnet keys are stored unencrypted: never use them for real funds.

#![forbid(unsafe_code)]

mod app;
mod genesis_json;
mod hex;
mod rpc;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine;
use ed25519_dalek::SigningKey;
use tenebra_chain::{Address, Amount, Tx, TxKind, NATIVE};

use crate::genesis_json::{AppGenesis, BalanceJson, ParamsJson, ValidatorJson};

const USAGE: &str = "\
tenebra-node — Tenebra L1 node and devnet wallet

USAGE:
  tenebra-node start [--abci 127.0.0.1:26658]
      Run the ABCI application; start CometBFT next to it.

  tenebra-node init-devnet --home <cometbft home> [--home <home2> ...]
                           [--keys ./devnet-keys] [--symbol TENEBERA] [--chain-id 1]
      Write Tenebra's genesis into each CometBFT home (after `cometbft init`),
      using each home's validator key. Creates operator keys and a funded
      account `alice` in the keys directory.

  tenebra-node keygen --out <file>         New account key (devnet only).
  tenebra-node address --key <file>        Print an account's address.

  tenebra-node query <path> [--rpc http://127.0.0.1:26657]
      Paths: summary | balance/<addr> | balance/<addr>/<asset> | nonce/<addr>
             | validator/<addr> | delegation/<delegator>/<validator>

  tenebra-node tx <kind> --key <file> [--rpc URL] ...
      transfer   --to <addr> --amount <coins> [--asset <id>]
      delegate   --validator <addr> --amount <coins>
      undelegate --validator <addr> --amount <coins>
      claim      --validator <addr>
      unjail

Amounts are in whole coins with up to `decimals` fractional digits, e.g. 12.5.
";

struct Args(Vec<String>);

impl Args {
    fn opt(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.0.get(i + 1))
            .map(String::as_str)
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .windows(2)
            .filter(|w| w[0] == name)
            .map(|w| w[1].as_str())
            .collect()
    }
    fn req(&self, name: &str) -> Result<&str, String> {
        self.opt(name).ok_or_else(|| format!("missing {name}"))
    }
    fn rpc(&self) -> &str {
        self.opt("--rpc").unwrap_or("http://127.0.0.1:26657")
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args(argv.clone());
    let res = match argv.first().map(String::as_str) {
        Some("start") => start(&args),
        Some("init-devnet") => init_devnet(&args),
        Some("keygen") => args
            .req("--out")
            .and_then(|p| keygen(Path::new(p)))
            .map(|a| println!("{}", hex::encode(&a.0))),
        Some("address") => args
            .req("--key")
            .and_then(|p| load_key(Path::new(p)))
            .map(|k| println!("{}", hex::encode(&address(&k).0))),
        Some("query") => argv
            .get(1)
            .ok_or_else(|| "missing query path".to_string())
            .and_then(|p| {
                rpc::abci_query(args.rpc(), p)
                    .map(|v| println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()))
            }),
        Some("tx") => send_tx(&args, argv.get(1).map(String::as_str).unwrap_or("")),
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn start(args: &Args) -> Result<(), String> {
    let addr = args.opt("--abci").unwrap_or("127.0.0.1:26658");
    eprintln!(
        "tenebra-node: ABCI listening on {addr} (start CometBFT with --proxy_app tcp://{addr})"
    );
    tendermint_abci::ServerBuilder::default()
        .bind(addr, app::TenebraApp::default())
        .map_err(|e| e.to_string())?
        .listen()
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------ keys

#[derive(serde::Serialize, serde::Deserialize)]
struct KeyFile {
    secret: String,
    address: String,
}

fn address(k: &SigningKey) -> Address {
    Address(k.verifying_key().to_bytes())
}

fn keygen(path: &Path) -> Result<Address, String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
    let k = SigningKey::from_bytes(&seed);
    let f = KeyFile {
        secret: hex::encode(&seed),
        address: hex::encode(&address(&k).0),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&f).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(address(&k))
}

fn load_key(path: &Path) -> Result<SigningKey, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let f: KeyFile = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    hex::decode32(&f.secret)
        .map(|b| SigningKey::from_bytes(&b))
        .ok_or_else(|| "bad secret in key file".into())
}

// ------------------------------------------------------------ devnet genesis

fn validator_pubkey(home: &Path) -> Result<[u8; 32], String> {
    let p = home.join("config/priv_validator_key.json");
    let s = std::fs::read_to_string(&p)
        .map_err(|e| format!("{}: {e} (run `cometbft init` first)", p.display()))?;
    let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    let b64 = v["pub_key"]["value"]
        .as_str()
        .ok_or("pub_key.value missing")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    bytes
        .try_into()
        .map_err(|_| "validator key is not 32 bytes (ed25519 expected)".into())
}

fn init_devnet(args: &Args) -> Result<(), String> {
    let homes: Vec<PathBuf> = args.all("--home").into_iter().map(PathBuf::from).collect();
    if homes.is_empty() {
        return Err("at least one --home is required".into());
    }
    let keys = PathBuf::from(args.opt("--keys").unwrap_or("./devnet-keys"));
    let symbol = args.opt("--symbol").unwrap_or("TENEBERA");
    let chain_id: u64 = args
        .opt("--chain-id")
        .unwrap_or("1")
        .parse()
        .map_err(|_| "bad --chain-id")?;
    let coin: Amount = 1_000_000_000;

    let mut balances = vec![];
    let mut validators = vec![];
    for (i, home) in homes.iter().enumerate() {
        let cons = validator_pubkey(home)?;
        let op_file = keys.join(format!("validator{i}.json"));
        let op = if op_file.exists() {
            address(&load_key(&op_file)?)
        } else {
            keygen(&op_file)?
        };
        balances.push(BalanceJson {
            address: hex::encode(&op.0),
            asset: NATIVE,
            amount: (1_000 * coin).to_string(),
        });
        validators.push(ValidatorJson {
            operator: hex::encode(&op.0),
            consensus_key: hex::encode(&cons),
            self_bond: (10_000 * coin).to_string(),
            commission_bps: 500,
        });
    }
    let alice_file = keys.join("alice.json");
    let alice = if alice_file.exists() {
        address(&load_key(&alice_file)?)
    } else {
        keygen(&alice_file)?
    };
    balances.push(BalanceJson {
        address: hex::encode(&alice.0),
        asset: NATIVE,
        amount: (1_000_000 * coin).to_string(),
    });

    let app_state = AppGenesis {
        params: ParamsJson::devnet(chain_id, symbol),
        assets: vec![],
        balances,
        validators,
    };
    app_state.to_genesis(0)?; // validate before writing anything
    let app_state = serde_json::to_value(&app_state).map_err(|e| e.to_string())?;

    for home in &homes {
        let p = home.join("config/genesis.json");
        let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut g: serde_json::Value = serde_json::from_str(&s).map_err(|e| e.to_string())?;
        g["app_state"] = app_state.clone();
        std::fs::write(
            &p,
            serde_json::to_string_pretty(&g).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        eprintln!("wrote app_state to {}", p.display());
    }
    eprintln!(
        "keys in {}: alice = {}",
        keys.display(),
        hex::encode(&alice.0)
    );
    Ok(())
}

// ------------------------------------------------------------ transactions

fn parse_coins(s: &str, decimals: u32) -> Result<Amount, String> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > decimals as usize || whole.is_empty() && frac.is_empty() {
        return Err(format!("bad amount {s:?}"));
    }
    let whole: Amount = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| format!("bad amount {s:?}"))?
    };
    let frac_val: Amount = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| format!("bad amount {s:?}"))?
    };
    let scale = 10u128.pow(decimals);
    let frac_digits = u32::try_from(frac.len()).map_err(|_| format!("bad amount {s:?}"))?;
    let frac_scaled = frac_val * 10u128.pow(decimals - frac_digits);
    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac_scaled))
        .ok_or_else(|| "amount too large".into())
}

fn send_tx(args: &Args, kind: &str) -> Result<(), String> {
    let key = load_key(Path::new(args.req("--key")?))?;
    let sender = address(&key);
    let summary = rpc::abci_query(args.rpc(), "summary")?;
    let chain_id = summary["chain_id"]
        .as_u64()
        .ok_or("summary.chain_id missing")?;
    let decimals = summary["decimals"]
        .as_u64()
        .and_then(|d| u32::try_from(d).ok())
        .unwrap_or(9);
    let base_fee: Amount = summary["base_fee"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or("summary.base_fee missing")?;
    let nonce = rpc::abci_query(args.rpc(), &format!("nonce/{}", hex::encode(&sender.0)))?["nonce"]
        .as_u64()
        .ok_or("nonce missing")?;
    let addr_arg = |n: &str| -> Result<Address, String> {
        hex::decode32(args.req(n)?)
            .map(Address)
            .ok_or_else(|| format!("{n}: bad address"))
    };
    let coins = || parse_coins(args.req("--amount")?, decimals);

    let kind = match kind {
        "transfer" => TxKind::Transfer {
            to: addr_arg("--to")?,
            asset: args
                .opt("--asset")
                .map_or(Ok(NATIVE), |a| a.parse().map_err(|_| "bad --asset"))?,
            amount: coins()?,
        },
        "delegate" => TxKind::Delegate {
            validator: addr_arg("--validator")?,
            amount: coins()?,
        },
        "undelegate" => TxKind::Undelegate {
            validator: addr_arg("--validator")?,
            amount: coins()?,
        },
        "claim" => TxKind::ClaimRewards {
            validator: addr_arg("--validator")?,
        },
        "unjail" => TxKind::Unjail,
        other => return Err(format!("unknown tx kind {other:?}\n\n{USAGE}")),
    };
    // Accept up to 2× today's base fee; the chain charges the actual base fee.
    let tx = Tx::signed(chain_id, &key, nonce, base_fee.saturating_mul(2), kind);
    let res = rpc::broadcast_commit(args.rpc(), &tx.encode())?;
    println!("{}", serde_json::to_string_pretty(&res).unwrap_or_default());
    let ok =
        res["check_tx"]["code"].as_u64() == Some(0) && res["tx_result"]["code"].as_u64() == Some(0);
    if ok {
        Ok(())
    } else {
        Err("transaction failed (see log above)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_coins;

    #[test]
    fn coin_amounts_parse_exactly() {
        assert_eq!(parse_coins("12.5", 9), Ok(12_500_000_000));
        assert_eq!(parse_coins("7", 9), Ok(7_000_000_000));
        assert_eq!(parse_coins("0.000000001", 9), Ok(1));
        assert_eq!(parse_coins(".5", 9), Ok(500_000_000));
        for bad in ["", ".", "1.0000000001", "abc", "1.2.3", "-1"] {
            assert!(parse_coins(bad, 9).is_err(), "{bad:?} should be rejected");
        }
    }
}
