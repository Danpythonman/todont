//! Domain logic. Knows nothing about terminals, argv, or formatting.
//!
//! Rules enforced here:
//!   * never `println!` / `eprintln!`, which would corrupt the TUI's screen
//!   * never return pre-formatted strings; frontends do the rendering

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use rusqlite::Connection;

use crate::alerts::{Nag, Remind};
use crate::db;

pub use crate::db::Task;
use crate::proto::{Change, SyncResponse};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("no task with id {0}")]
    NotFound(i64),

    #[error("title must not be empty")]
    EmptyTitle,

    #[error("{0}")]
    BadDue(#[from] crate::due::DueError),

    #[error("creating {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
}

impl CoreError {
    /// Frontends decide policy; core only suggests. The CLI uses this for
    /// `std::process::exit`, the TUI ignores it.
    pub fn exit_code(&self) -> i32 {
        match self {
            CoreError::EmptyTitle | CoreError::BadDue(_) => 2,
            CoreError::NotFound(_) => 4,
            CoreError::Db(_) => 65,     // EX_DATAERR
            CoreError::Io { .. } => 74, // EX_IOERR
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Fields to change in [`App::edit`]; `None` leaves a field alone.
#[derive(Debug, Default)]
pub struct Edit {
    pub title: Option<String>,
    /// `Some(None)` clears the due date.
    pub due: Option<Option<Timestamp>>,
    pub remind: Option<Remind>,
    pub nag: Option<Nag>,
}

/// Everything needed to create a task; see [`App::create`].
#[derive(Debug, Default)]
pub struct NewTask<'a> {
    pub title: &'a str,
    pub due: Option<Timestamp>,
    pub remind: Remind,
    pub nag: Nag,
}

/// The single backend both frontends talk to.
pub struct App {
    conn: Connection,
}

impl App {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| CoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        Self::from_conn(db::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(mut conn: Connection) -> Result<Self> {
        db::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// `$TODONT_DB`, else `$XDG_DATA_HOME/todont/todont.db`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("TODONT_DB") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("todont/todont.db")
    }

    // ---- queries ---------------------------------------------------------

    /// Live tasks: pending first, then by due date (undated last).
    pub fn tasks(&self, include_done: bool) -> Result<Vec<Task>> {
        Ok(db::get_tasks(&self.conn, include_done)?)
    }

    pub fn get(&self, id: i64) -> Result<Task> {
        db::get_task_by_id(&self.conn, id)?.ok_or(CoreError::NotFound(id))
    }

    // ---- commands --------------------------------------------------------

    #[cfg(test)]
    pub fn add(
        &mut self,
        title: &str,
        due: Option<Timestamp>,
    ) -> Result<Task> {
        self.create(NewTask {
            title,
            due,
            ..Default::default()
        })
    }

    pub fn create(&mut self, new: NewTask) -> Result<Task> {
        let title = clean_title(new.title)?;
        let uuid = uuid::Uuid::new_v4().to_string();
        let id = db::insert_task(
            &self.conn,
            &uuid,
            title,
            new.due.map(|t| t.as_second()),
            new.remind,
            new.nag,
            now_ms(),
        )?;
        self.get(id)
    }

    pub fn set_done(&mut self, id: i64, done: bool) -> Result<Task> {
        let mut task = self.get(id)?;
        task.done = done;
        self.save(task)
    }

    pub fn edit(&mut self, id: i64, edit: Edit) -> Result<Task> {
        let mut task = self.get(id)?;
        if let Some(title) = &edit.title {
            task.title = clean_title(title)?.to_string();
        }
        if let Some(due) = edit.due {
            task.due = due.map(|t| t.as_second());
        }
        if let Some(remind) = edit.remind {
            task.remind = remind;
        }
        if let Some(nag) = edit.nag {
            task.nag = nag;
        }
        self.save(task)
    }

    pub fn remove(&mut self, id: i64) -> Result<Task> {
        let task = self.get(id)?;
        db::delete_task(&self.conn, id, now_ms())?;
        Ok(task)
    }

    fn save(&mut self, mut task: Task) -> Result<Task> {
        // Never move the clock backwards, even if the system clock did, so
        // this edit still beats the one it replaces during sync.
        task.updated = now_ms().max(task.updated + 1);
        task.dirty = true;
        if !db::update_task(&self.conn, &task)? {
            return Err(CoreError::NotFound(task.id));
        }
        Ok(task)
    }
}

/// Sync bookkeeping. The HTTP side lives in `sync.rs`; these only touch the
/// database.
impl App {
    /// The server id and cursor from the last successful sync.
    pub fn sync_state(&self) -> Result<(Option<String>, i64)> {
        let id = db::get_meta(&self.conn, "server_id")?;
        let cursor = db::get_meta(&self.conn, "cursor")?
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        Ok((id, cursor))
    }

    /// Local changes not yet pushed, including deletions.
    pub fn pending_changes(&self) -> Result<Vec<Change>> {
        Ok(db::get_dirty(&self.conn)?)
    }

    /// Records a successful sync: `pushed` reached the server, and the
    /// server replied with `resp`. Returns how many remote changes landed.
    pub fn apply_sync(
        &mut self,
        pushed: &[Change],
        resp: &SyncResponse,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        for c in pushed {
            db::clear_dirty(&tx, c)?;
        }
        let mut pulled = 0;
        for c in &resp.changes {
            pulled += db::apply_remote(&tx, c)? as usize;
        }
        db::set_meta(&tx, "server_id", &resp.server_id)?;
        db::set_meta(&tx, "cursor", &resp.cursor.to_string())?;
        tx.commit()?;
        Ok(pulled)
    }

    /// Forgets all sync progress so the next sync pushes every task and
    /// pulls everything, e.g. after the server's database was replaced.
    pub fn reset_sync(&mut self) -> Result<()> {
        Ok(db::mark_all_dirty(&self.conn)?)
    }
}

fn clean_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() {
        return Err(CoreError::EmptyTitle);
    }
    Ok(title)
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::open_in_memory().unwrap()
    }

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_second(s).unwrap()
    }

    #[test]
    fn add_then_complete() {
        let mut app = app();
        let t = app.add("  buy milk ", None).unwrap();
        assert_eq!(t.title, "buy milk");
        assert!(!t.done);
        let t = app.set_done(t.id, true).unwrap();
        assert!(t.done);
        assert!(app.tasks(false).unwrap().is_empty());
        assert_eq!(app.tasks(true).unwrap().len(), 1);
    }

    #[test]
    fn empty_title_rejected() {
        let mut app = app();
        assert!(matches!(app.add("   ", None), Err(CoreError::EmptyTitle)));
    }

    #[test]
    fn missing_id_reports_not_found() {
        let mut app = app();
        assert!(matches!(
            app.set_done(99, true),
            Err(CoreError::NotFound(99))
        ));
    }

    #[test]
    fn removed_tasks_are_hidden() {
        let mut app = app();
        let t = app.add("x", None).unwrap();
        app.remove(t.id).unwrap();
        assert!(app.tasks(true).unwrap().is_empty());
        assert!(matches!(app.get(t.id), Err(CoreError::NotFound(_))));
    }

    #[test]
    fn ordering_is_pending_then_due_then_undated() {
        let mut app = app();
        let undated = app.add("undated", None).unwrap();
        let late = app.add("late", Some(ts(2_000))).unwrap();
        let early = app.add("early", Some(ts(1_000))).unwrap();
        let done = app.add("done", Some(ts(500))).unwrap();
        app.set_done(done.id, true).unwrap();
        let ids: Vec<i64> =
            app.tasks(true).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, [early.id, late.id, undated.id, done.id]);
    }

    #[test]
    fn alerts_are_stored_and_editable() {
        let mut app = app();
        let t = app
            .create(NewTask {
                title: "x",
                due: Some(ts(1_000)),
                remind: Remind::Before(15),
                nag: Nag::Off,
            })
            .unwrap();
        assert_eq!((t.remind, t.nag), (Remind::Before(15), Nag::Off));
        let edit = Edit {
            nag: Some(Nag::Every(30)),
            ..Default::default()
        };
        let t = app.edit(t.id, edit).unwrap();
        assert_eq!((t.remind, t.nag), (Remind::Before(15), Nag::Every(30)));
    }

    #[test]
    fn edit_changes_and_clears_due() {
        let mut app = app();
        let t = app.add("x", Some(ts(1_000))).unwrap();
        let before = t.updated;
        let t = app
            .edit(
                t.id,
                Edit {
                    title: Some("y".into()),
                    due: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(t.title, "y");
        assert_eq!(t.due, None);
        assert!(t.updated > before);
    }
}
