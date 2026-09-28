//! Wire format for `POST /sync`, shared by client and server.
//!
//! The client sends every task it changed since its last sync, plus the
//! server `cursor` it last saw. The server keeps whichever version of each
//! task has the newer `updated`, then returns every task that changed after
//! `cursor` along with the new cursor.

use serde::{Deserialize, Serialize};

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
