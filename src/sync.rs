//! Sync client: pushes local changes to the server and pulls everyone
//! else's. Blocking, with a short timeout so an offline laptop never hangs.

use std::time::Duration;

use crate::config::SyncConfig;
use crate::core::{App, CoreError};
use crate::proto::{SyncRequest, SyncResponse};

pub const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("server rejected the sync token")]
    Unauthorized,
    #[error("can't reach sync server: {0}")]
    Http(#[from] ureq::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pushed: usize,
    pub pulled: usize,
}

pub fn sync(app: &mut App, cfg: &SyncConfig) -> Result<Stats, SyncError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let url = format!("{}/sync", cfg.server.trim_end_matches('/'));

    let (known_server, cursor) = app.sync_state()?;
    let changes = app.pending_changes()?;
    let resp: SyncResponse = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.token))
        .send_json(SyncRequest {
            cursor,
            changes: changes.clone(),
        })
        .map_err(|e| match e {
            ureq::Error::StatusCode(401) => SyncError::Unauthorized,
            e => SyncError::Http(e),
        })?
        .body_mut()
        .read_json()?;

    if known_server.is_some_and(|id| id != resp.server_id) {
        // A different server database (restored, wiped, or moved): our
        // cursor means nothing there, so start over and push everything.
        // After the reset there's no known id, so this recurses once.
        app.reset_sync()?;
        return sync(app, cfg);
    }
    let pulled = app.apply_sync(&changes, &resp)?;
    Ok(Stats {
        pushed: changes.len(),
        pulled,
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::core::Edit;
    use crate::server;

    const TOKEN: &str = "test-token";

    /// Starts a server on a random port with an in-memory database.
    fn start_server() -> SocketAddr {
        let conn = server::open_db_in_memory().unwrap();
        let state = server::Shared::new(conn, TOKEN.into());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, server::router(state)).await.unwrap();
            });
        });
        rx.recv().unwrap()
    }

    fn cfg(addr: SocketAddr) -> SyncConfig {
        SyncConfig {
            server: format!("http://{addr}/"),
            token: TOKEN.into(),
        }
    }

    fn titles(app: &App) -> Vec<String> {
        app.tasks(true)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect()
    }

    #[test]
    fn two_devices_converge() {
        let cfg = cfg(start_server());
        let mut laptop = App::open_in_memory().unwrap();
        let mut desktop = App::open_in_memory().unwrap();

        let milk = laptop.add("milk", None).unwrap();
        laptop.add("eggs", None).unwrap();
        let s = sync(&mut laptop, &cfg).unwrap();
        assert_eq!(
            s,
            Stats {
                pushed: 2,
                pulled: 0
            }
        );

        let s = sync(&mut desktop, &cfg).unwrap();
        assert_eq!(
            s,
            Stats {
                pushed: 0,
                pulled: 2
            }
        );
        let mut t = titles(&desktop);
        t.sort();
        assert_eq!(t, ["eggs", "milk"]);

        // Desktop completes milk; laptop deletes eggs.
        let d_milk = desktop
            .tasks(true)
            .unwrap()
            .into_iter()
            .find(|t| t.uuid == milk.uuid)
            .unwrap();
        desktop.set_done(d_milk.id, true).unwrap();
        let eggs = laptop.tasks(true).unwrap();
        let eggs = eggs.iter().find(|t| t.title == "eggs").unwrap();
        laptop.remove(eggs.id).unwrap();

        sync(&mut desktop, &cfg).unwrap();
        sync(&mut laptop, &cfg).unwrap();
        sync(&mut desktop, &cfg).unwrap();

        for app in [&laptop, &desktop] {
            let tasks = app.tasks(true).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].uuid, milk.uuid);
            assert!(tasks[0].done);
        }
        // Nothing left to push anywhere.
        assert!(laptop.pending_changes().unwrap().is_empty());
        assert!(desktop.pending_changes().unwrap().is_empty());
    }

    #[test]
    fn newer_edit_wins() {
        let cfg = cfg(start_server());
        let mut a = App::open_in_memory().unwrap();
        let mut b = App::open_in_memory().unwrap();
        let t = a.add("original", None).unwrap();
        sync(&mut a, &cfg).unwrap();
        sync(&mut b, &cfg).unwrap();
        let bt = b.tasks(true).unwrap()[0].id;

        let edit = |s: &str| Edit {
            title: Some(s.into()),
            ..Default::default()
        };
        a.edit(t.id, edit("from a")).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        b.edit(bt, edit("from b")).unwrap();

        // b's edit is newer, so it wins even though a syncs last.
        sync(&mut b, &cfg).unwrap();
        sync(&mut a, &cfg).unwrap();
        sync(&mut b, &cfg).unwrap();
        assert_eq!(titles(&a), ["from b"]);
        assert_eq!(titles(&b), ["from b"]);
    }

    #[test]
    fn replaced_server_gets_everything_again() {
        let mut app = App::open_in_memory().unwrap();
        app.add("keep me", None).unwrap();
        sync(&mut app, &cfg(start_server())).unwrap();

        // A brand-new server (e.g. its database was lost).
        let fresh = cfg(start_server());
        let s = sync(&mut app, &fresh).unwrap();
        assert_eq!(s.pushed, 1);
        let mut other = App::open_in_memory().unwrap();
        sync(&mut other, &fresh).unwrap();
        assert_eq!(titles(&other), ["keep me"]);
    }

    #[test]
    fn bad_token_is_reported() {
        let addr = start_server();
        let bad = SyncConfig {
            token: "nope".into(),
            ..cfg(addr)
        };
        let mut app = App::open_in_memory().unwrap();
        assert!(matches!(sync(&mut app, &bad), Err(SyncError::Unauthorized)));
    }
}
