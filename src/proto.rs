//! Wire format for `POST /sync`, shared by client and server.
//!
//! The client sends every task it changed since its last sync, plus the
//! server `cursor` it last saw. The server keeps whichever version of each
//! task has the newer `updated`, then returns every task that changed after
//! `cursor` along with the new cursor.

use serde::{Deserialize, Serialize};

/// The sync protocol this build speaks. Bump it only when an older client
/// or server could no longer sync correctly with this one; adding a field
/// with `#[serde(default)]` doesn't need a bump.
pub const PROTOCOL: u32 = 1;

/// What a peer that sends no protocol header speaks: todont 0.1.0, from
/// before the headers existed.
pub const UNLABELLED_PROTOCOL: u32 = 1;

/// This build's version, e.g. "0.2.0".
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Request and response headers carrying `PROTOCOL` and `VERSION`.
pub const PROTOCOL_HEADER: &str = "todont-protocol";
pub const VERSION_HEADER: &str = "todont-version";

/// Body of a `426 Upgrade Required` reply to a client whose protocol
/// differs from the server's.
#[derive(Debug, Serialize, Deserialize)]
pub struct Incompatible {
    pub server_version: String,
    pub server_protocol: u32,
}

/// Parses a protocol header; absent or garbled means a 0.1.0 peer.
pub fn parse_protocol(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(UNLABELLED_PROTOCOL)
}

/// Whether version `a` is newer than `b` ("0.10.0" > "0.9.3"). Anything
/// that isn't plain numbers compares as equal.
pub fn is_newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.split('.').map(|p| p.parse().ok()).collect()
    }
    match (parts(a), parts(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// A full snapshot of one task. Deletions are sent as tombstones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub uuid: String,
    pub title: String,
    pub done: bool,
    /// Unix seconds.
    pub due: Option<i64>,
    /// Unix millis.
    pub created: i64,
    /// Unix millis; the last-write-wins clock.
    pub updated: i64,
    pub deleted: bool,
    /// Raw `alerts::Remind`; absent from older clients.
    #[serde(default)]
    pub remind: Option<i64>,
    /// Raw `alerts::Nag`; absent from older clients.
    #[serde(default)]
    pub nag: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncRequest {
    pub cursor: i64,
    pub changes: Vec<Change>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Random per server database; if it changes, the server was replaced
    /// and the client must push everything again.
    pub server_id: String,
    pub cursor: i64,
    pub changes: Vec<Change>,
}

impl Change {
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Change {
            uuid: row.get(0)?,
            title: row.get(1)?,
            done: row.get(2)?,
            due: row.get(3)?,
            created: row.get(4)?,
            updated: row.get(5)?,
            deleted: row.get(6)?,
            remind: row.get(7)?,
            nag: row.get(8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0-beta", "0.1.0"));
    }

    #[test]
    fn missing_protocol_means_0_1_0() {
        assert_eq!(parse_protocol(None), UNLABELLED_PROTOCOL);
        assert_eq!(parse_protocol(Some("junk")), UNLABELLED_PROTOCOL);
        assert_eq!(parse_protocol(Some(" 7 ")), 7);
    }
}
