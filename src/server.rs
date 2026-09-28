//! `td serve`: the sync hub every device talks to, and the home of the ntfy
//! notifier (the one process that's always on).

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use rusqlite::{Connection, params};

use crate::config::Config;
use crate::notify;
use crate::proto::{Change, SyncRequest, SyncResponse};

const MIGRATIONS: &[&str] = &[
    include_str!("sql/server/schema.sql"),
    include_str!("sql/server/migrate_2.sql"),
];
const UPSERT_TASK: &str = include_str!("sql/server/upsert_task.sql");
const CHANGES_SINCE: &str = include_str!("sql/server/changes_since.sql");

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("timezone {name:?}: {source}")]
    Tz {
        name: String,
        #[source]
        source: jiff::Error,
    },
}

pub fn open_db(path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        // Let SQLite report the real error if this fails.
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // The notifier thread has its own connection to the same file.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init(conn)
}

#[cfg(test)]
pub fn open_db_in_memory() -> rusqlite::Result<Connection> {
    init(Connection::open_in_memory()?)
}

fn init(mut conn: Connection) -> rusqlite::Result<Connection> {
    let version: usize =
        conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('server_id', ?1)",
        params![uuid::Uuid::new_v4().to_string()],
    )?;
    Ok(conn)
}

/// Applies a client's changes and returns what it's missing. One
/// transaction, so concurrent clients see consistent cursors.
pub fn apply(
    conn: &mut Connection,
    req: &SyncRequest,
) -> rusqlite::Result<SyncResponse> {
    let tx = conn.transaction()?;
    let mut seq: i64 =
        tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM task", [], |r| {
            r.get(0)
        })?;
    {
        let mut upsert = tx.prepare_cached(UPSERT_TASK)?;
        for c in &req.changes {
            let n = upsert.execute(params![
                c.uuid,
                c.title,
                c.done,
                c.due,
                c.created,
                c.updated,
                c.deleted,
                c.remind,
                c.nag,
                seq + 1
            ])?;
            seq += n as i64;
        }
    }
    let changes = tx
        .prepare_cached(CHANGES_SINCE)?
        .query_map(params![req.cursor], Change::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let server_id = tx.query_row(
        "SELECT value FROM meta WHERE key = 'server_id'",
        [],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(SyncResponse {
        server_id,
        cursor: seq,
        changes,
    })
}

pub struct Shared {
    conn: Mutex<Connection>,
    token: String,
}

impl Shared {
    pub fn new(conn: Connection, token: String) -> Arc<Self> {
        Arc::new(Shared {
            conn: Mutex::new(conn),
            token,
        })
    }
}

pub fn router(state: Arc<Shared>) -> Router {
    Router::new()
        .route("/sync", post(sync))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        // Unauthenticated, for uptime checks.
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

async fn auth(
    State(state): State<Arc<Shared>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match given {
        Some(t) if constant_time_eq(t.as_bytes(), state.token.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn sync(
    State(state): State<Arc<Shared>>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let pushed = req.changes.len();
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = state.conn.lock().unwrap_or_else(|e| e.into_inner());
        apply(&mut conn, &req)
    })
    .await;
    match result {
        Ok(Ok(resp)) => {
            if pushed > 0 || !resp.changes.is_empty() {
                eprintln!(
                    "sync: {pushed} in, {} out, cursor {}",
                    resp.changes.len(),
                    resp.cursor
                );
            }
            Ok(Json(resp))
        }
        Ok(Err(e)) => {
            eprintln!("sync failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            eprintln!("sync task panicked: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Runs the server until SIGINT/SIGTERM.
pub fn run(config: &Config) -> Result<(), ServerError> {
    let sc = config.server()?;
    let conn = open_db(&sc.db)?;
    let state = Shared::new(conn, sc.token.clone());

    match &config.ntfy {
        Some(ntfy) => {
            let tz = match &sc.timezone {
                Some(name) => {
                    jiff::tz::TimeZone::get(name).map_err(|source| {
                        ServerError::Tz {
                            name: name.clone(),
                            source,
                        }
                    })?
                }
                None => jiff::tz::TimeZone::system(),
            };
            eprintln!(
                "notifying {}/{} (lead {}m, nag every {}m)",
                ntfy.url.trim_end_matches('/'),
                ntfy.topic,
                ntfy.lead_minutes,
                ntfy.nag_minutes
            );
            notify::spawn(open_db(&sc.db)?, ntfy.clone(), tz);
        }
        None => eprintln!("no [ntfy] section; notifications off"),
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&sc.listen).await?;
        eprintln!("todont server listening on {}", listener.local_addr()?);
        axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown_signal())
            .await
    })?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    eprintln!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(uuid: &str, title: &str, updated: i64) -> Change {
        Change {
            uuid: uuid.into(),
            title: title.into(),
            done: false,
            due: None,
            created: 0,
            updated,
            deleted: false,
            remind: None,
            nag: None,
        }
    }

    fn push(
        conn: &mut Connection,
        cursor: i64,
        c: Vec<Change>,
    ) -> SyncResponse {
        apply(conn, &SyncRequest { cursor, changes: c }).unwrap()
    }

    #[test]
    fn older_writes_are_ignored_and_dont_bump_cursor() {
        let mut conn = open_db_in_memory().unwrap();
        let r = push(&mut conn, 0, vec![change("a", "new", 10)]);
        assert_eq!(r.cursor, 1);
        let r = push(&mut conn, 1, vec![change("a", "old", 5)]);
        assert_eq!(r.cursor, 1);
        assert!(r.changes.is_empty());
        let r = push(&mut conn, 0, vec![]);
        assert_eq!(r.changes[0].title, "new");
    }

    #[test]
    fn server_id_is_stable() {
        let mut conn = open_db_in_memory().unwrap();
        let a = push(&mut conn, 0, vec![]).server_id;
        let b = push(&mut conn, 0, vec![]).server_id;
        assert_eq!(a, b);
    }
}
