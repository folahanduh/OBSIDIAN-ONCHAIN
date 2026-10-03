//! Minimal client for CometBFT's local HTTP RPC (plain HTTP/1.0 over TCP,
//! which avoids chunked responses). Intended for a node on localhost.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine;

use crate::hex;

fn get(rpc: &str, path_and_query: &str) -> Result<serde_json::Value, String> {
    let hostport = rpc
        .strip_prefix("http://")
        .ok_or("only http:// RPC endpoints are supported")?
        .trim_end_matches('/');
    let mut s = TcpStream::connect(hostport)
        .map_err(|e| format!("cannot reach CometBFT RPC at {rpc}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    write!(
        s,
        "GET {path_and_query} HTTP/1.0\r\nHost: {hostport}\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .ok_or("malformed HTTP response")?;
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("bad JSON from RPC: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("RPC error: {err}"));
    }
    Ok(v["result"].clone())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Query the application (`abci_query`); returns the decoded JSON value.
pub fn abci_query(rpc: &str, path: &str) -> Result<serde_json::Value, String> {
    let r = get(
        rpc,
        &format!("/abci_query?path={}", urlencode(&format!("\"{path}\""))),
    )?;
    let resp = &r["response"];
    if resp["code"].as_u64().unwrap_or(1) != 0 {
        return Err(format!(
            "query {path}: {}",
            resp["log"].as_str().unwrap_or("failed")
        ));
    }
    let b64 = resp["value"].as_str().unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Broadcast a transaction and wait until it is committed in a block.
pub fn broadcast_commit(rpc: &str, tx: &[u8]) -> Result<serde_json::Value, String> {
    get(
        rpc,
        &format!("/broadcast_tx_commit?tx=0x{}", hex::encode(tx)),
    )
}
