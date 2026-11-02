#![doc = include_str!("../README.md")]
pub mod catalog;
pub mod client;
pub mod store;
pub mod tools;
pub use client::{Client, DEFAULT_BASE_URL, Output};
use serde_json::Value;
pub use store::{Credentials, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("offline: network access is disabled")]
    Offline,
    #[error("validation: {0}")]
    Validation(&'static str),
    #[error("unauthenticated: login required or rejected")]
    Unauthenticated,
    #[error("two-factor authentication is enabled for this account and is not supported")]
    TwoFactor,
    #[error("the portal privacy policy has not been accepted; accept it in the portal first")]
    PrivacyPolicy,
    #[error("HTTP request failed")]
    Transport,
    #[error("portal HTTP status {0}")]
    Http(u16),
    #[error("portal API rejected the request")]
    Api,
    #[error("invalid portal response")]
    Parse,
    #[error("response exceeds size limit")]
    TooLarge,
    #[error("private store I/O failed")]
    Io(#[from] std::io::Error),
    #[error("encrypted store could not be authenticated")]
    Crypto,
    #[error("not_available")]
    NotAvailable,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Splits an API `Content` value into rows: an array yields its items, `null` yields none, and any
/// other value is a single row. The portal's response shapes are not documented, so none is assumed.
pub fn rows(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}
fn row_key(row: &Value) -> String {
    // serde_json orders object keys, so equal rows serialize identically.
    serde_json::to_string(row).expect("serializable row")
}
/// Rows in `current` that are not present, byte for byte, in `previous`.
pub fn check_new_bills(current: &[Value], previous: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashSet<_> = previous.iter().map(row_key).collect();
    current
        .iter()
        .filter(|r| seen.insert(row_key(r)))
        .cloned()
        .collect()
}
pub fn marshal_jsonl(rows: &[Value]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).map_err(|_| Error::Parse)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}
pub fn unmarshal_jsonl(bytes: &[u8]) -> Result<Vec<Value>> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .map(|l| serde_json::from_slice(l).map_err(|_| Error::Parse))
        .collect()
}
