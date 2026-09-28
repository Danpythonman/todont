//! Thin typed layer over the local SQLite database. One function per query
//! file in `sql/`; no domain rules live here.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::alerts::{Nag, Remind};
use crate::proto::Change;

/// A task as stored locally. `id` is a per-device handle for the CLI/TUI;
/// `uuid` identifies the task across devices.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Task {
    pub id: i64,
    pub uuid: String,
    pub title: String,
    pub done: bool,
    /// Unix seconds.
    pub due: Option<i64>,
    /// Unix millis.
    pub created: i64,
    /// Unix millis; the last-write-wins clock used by sync.
    pub updated: i64,
    pub remind: Remind,
    pub nag: Nag,
    /// Changed here but not yet pushed to the sync server.
    #[serde(skip)]
    pub dirty: bool,
}

/// Each entry upgrades the schema by one version; `PRAGMA user_version`
/// records how many have been applied.
const MIGRATIONS: &[&str] = &[
    include_str!("sql/schema.sql"),
    include_str!("sql/migrate_2.sql"),
];

const GET_TASK_BY_ID: &str = include_str!("sql/get_task_by_id.sql");
const GET_TASKS: &str = include_str!("sql/get_tasks.sql");
const INSERT_TASK: &str = include_str!("sql/insert_task.sql");
const UPDATE_TASK: &str = include_str!("sql/update_task.sql");
const DELETE_TASK: &str = include_str!("sql/delete_task.sql");

pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // The CLI, TUI and background sync may touch the file at the same time.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: usize =
        conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

fn task_from_row(row: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        uuid: row.get(1)?,
        title: row.get(2)?,
        done: row.get(3)?,
        due: row.get(4)?,
        created: row.get(5)?,
        updated: row.get(6)?,
        remind: Remind::from_raw(row.get(7)?),
        nag: Nag::from_raw(row.get(8)?),
        dirty: row.get(9)?,
    })
}

pub fn get_task_by_id(
    conn: &Connection,
    id: i64,
) -> rusqlite::Result<Option<Task>> {
    conn.query_row(GET_TASK_BY_ID, params![id], task_from_row)
        .optional()
}

pub fn get_tasks(
    conn: &Connection,
    include_done: bool,
) -> rusqlite::Result<Vec<Task>> {
    let mut stmt = conn.prepare_cached(GET_TASKS)?;
    let rows = stmt.query_map(params![include_done], task_from_row)?;
    rows.collect()
}

pub fn insert_task(
    conn: &Connection,
    uuid: &str,
    title: &str,
    due: Option<i64>,
    remind: Remind,
    nag: Nag,
    now_ms: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        INSERT_TASK,
        params![uuid, title, due, now_ms, remind.to_raw(), nag.to_raw()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Writes every mutable field of `task`, marking it dirty for sync.
/// Returns false if no live task has that id.
pub fn update_task(conn: &Connection, task: &Task) -> rusqlite::Result<bool> {
    let n = conn.execute(
        UPDATE_TASK,
        params![
            task.id,
            task.title,
            task.done,
            task.due,
            task.updated,
            task.remind.to_raw(),
            task.nag.to_raw(),
        ],
    )?;
    Ok(n > 0)
}

/// Tombstones a task so the deletion can be synced.
pub fn delete_task(
    conn: &Connection,
    id: i64,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(DELETE_TASK, params![id, now_ms])? > 0)
}

// ---- sync ----------------------------------------------------------------

const GET_DIRTY: &str = include_str!("sql/get_dirty.sql");
const CLEAR_DIRTY: &str = include_str!("sql/clear_dirty.sql");
const APPLY_REMOTE: &str = include_str!("sql/apply_remote.sql");
const MARK_ALL_DIRTY: &str = include_str!("sql/mark_all_dirty.sql");
const GET_META: &str = include_str!("sql/get_meta.sql");
const SET_META: &str = include_str!("sql/set_meta.sql");

pub fn get_dirty(conn: &Connection) -> rusqlite::Result<Vec<Change>> {
    let mut stmt = conn.prepare_cached(GET_DIRTY)?;
    let rows = stmt.query_map([], Change::from_row)?;
    rows.collect()
}

pub fn clear_dirty(conn: &Connection, c: &Change) -> rusqlite::Result<()> {
    conn.execute(CLEAR_DIRTY, params![c.uuid, c.updated])?;
    Ok(())
}

/// Returns true if the change was newer than the local copy and landed.
pub fn apply_remote(conn: &Connection, c: &Change) -> rusqlite::Result<bool> {
    let n = conn.prepare_cached(APPLY_REMOTE)?.execute(params![
        c.uuid, c.title, c.done, c.due, c.created, c.updated, c.deleted,
        c.remind, c.nag
    ])?;
    Ok(n > 0)
}

pub fn mark_all_dirty(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(MARK_ALL_DIRTY)
}

pub fn get_meta(
    conn: &Connection,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(GET_META, params![key], |r| r.get(0))
        .optional()
}

pub fn set_meta(
    conn: &Connection,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    conn.execute(SET_META, params![key, value])?;
    Ok(())
}
