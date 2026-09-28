//! Sync client: pushes local changes to the server and pulls everyone
//! else's. Blocking, with a short timeout so an offline laptop never hangs.

use std::time::Duration;

use crate::config::SyncConfig;
use crate::core::{App, CoreError};
use crate::proto::{
    self, Incompatible, PROTOCOL, PROTOCOL_HEADER, SyncRequest, SyncResponse,
    VERSION, VERSION_HEADER,
};

pub const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("server rejected the sync token")]
    Unauthorized,
    #[error("{}", incompatible(.server_version, *.server_protocol, *.ours))]
    Incompatible {
        server_version: String,
        server_protocol: u32,
        ours: u32,
    },
    #[error("sync server replied with HTTP {0}")]
    Status(u16),
    #[error("can't reach sync server: {0}")]
    Http(#[from] ureq::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Says which side needs updating.
fn incompatible(
    server_version: &str,
    server_protocol: u32,
    ours: u32,
) -> String {
    let fix = if server_protocol > ours {
        "update this device: cargo install todont"
    } else {
        "update the server: cargo install todont, then restart td serve"
    };
    format!(
        "the server runs todont {server_version} (sync protocol \
         {server_protocol}) but this td is {VERSION} (protocol {ours}); {fix}"
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pushed: usize,
    pub pulled: usize,
    /// The server's todont version; `None` for 0.1.0, which didn't say.
    pub server_version: Option<String>,
}

impl Stats {
    /// A note for the user when this device and the server run different
    /// (but compatible) versions.
    pub fn version_note(&self) -> Option<String> {
        let server = self.server_version.as_deref().unwrap_or("0.1.0");
        if proto::is_newer(server, VERSION) {
            Some(format!(
                "the server runs todont {server}, this td is {VERSION}; \
                 update with: cargo install todont"
            ))
        } else if proto::is_newer(VERSION, server) {
            Some(format!(
                "this td is {VERSION} but the server runs todont {server}; \
                 update the server too"
            ))
        } else {
            None
        }
    }
}

pub fn sync(app: &mut App, cfg: &SyncConfig) -> Result<Stats, SyncError> {
    sync_as(app, cfg, PROTOCOL)
}

/// [`sync`], claiming to speak `protocol`; tests use it to impersonate
/// other versions.
fn sync_as(
    app: &mut App,
    cfg: &SyncConfig,
    protocol: u32,
) -> Result<Stats, SyncError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        // Error replies carry bodies we want to read (426).
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/sync", cfg.server.trim_end_matches('/'));

    let (known_server, cursor) = app.sync_state()?;
    let changes = app.pending_changes()?;
    let mut resp = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.token))
        .header(PROTOCOL_HEADER, &protocol.to_string())
        .header(VERSION_HEADER, VERSION)
        .send_json(SyncRequest {
            cursor,
            changes: changes.clone(),
        })?;
    let header = |name| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let server_protocol =
        proto::parse_protocol(header(PROTOCOL_HEADER).as_deref());
    let server_version = header(VERSION_HEADER);

    match resp.status().as_u16() {
        200..=299 => {}
        401 => return Err(SyncError::Unauthorized),
        426 => {
            let body: Incompatible = resp.body_mut().read_json()?;
            return Err(SyncError::Incompatible {
                server_version: body.server_version,
                server_protocol: body.server_protocol,
                ours: protocol,
            });
        }
        code => return Err(SyncError::Status(code)),
    }
    // A 0.1.0 server doesn't check versions, so check its reply here. (It
    // has already applied our changes by now; later servers refuse first.)
    if server_protocol != protocol {
        return Err(SyncError::Incompatible {
            server_version: server_version.unwrap_or_else(|| "0.1.0".into()),
            server_protocol,
            ours: protocol,
        });
    }
    let body: SyncResponse = resp.body_mut().read_json()?;

    if known_server.is_some_and(|id| id != body.server_id) {
        // A different server database (restored, wiped, or moved): our
        // cursor means nothing there, so start over and push everything.
        // After the reset there's no known id, so this recurses once.
        app.reset_sync()?;
        return sync_as(app, cfg, protocol);
    }
    let pulled = app.apply_sync(&changes, &body)?;
    Ok(Stats {
        pushed: changes.len(),
        pulled,
        server_version,
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
        start_with(server::router)
    }

    /// Like [`start_server`], but with a given router, e.g. one that
    /// behaves like todont 0.1.0.
    fn start_with(
        router: fn(std::sync::Arc<server::Shared>) -> axum::Router,
    ) -> SocketAddr {
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
                axum::serve(listener, router(state)).await.unwrap();
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
        assert_eq!((s.pushed, s.pulled), (2, 0));
        assert_eq!(s.server_version.as_deref(), Some(VERSION));
        assert_eq!(s.version_note(), None);

        let s = sync(&mut desktop, &cfg).unwrap();
        assert_eq!((s.pushed, s.pulled), (0, 2));
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

    #[test]
    fn mismatched_protocols_are_refused_before_any_change() {
        let cfg = cfg(start_server());
        let mut app = App::open_in_memory().unwrap();
        app.add("from the future", None).unwrap();

        let err = sync_as(&mut app, &cfg, PROTOCOL + 1).unwrap_err();
        assert!(
            matches!(err, SyncError::Incompatible { server_protocol, .. }
                if server_protocol == PROTOCOL),
            "{err}"
        );
        assert!(err.to_string().contains("update the server"), "{err}");
        // Nothing reached the server, and nothing was marked pushed.
        let mut other = App::open_in_memory().unwrap();
        assert_eq!(sync(&mut other, &cfg).unwrap().pulled, 0);
        assert_eq!(app.pending_changes().unwrap().len(), 1);

        let err = sync_as(&mut app, &cfg, PROTOCOL - 1).unwrap_err();
        assert!(err.to_string().contains("update this device"), "{err}");
    }

    #[test]
    fn a_0_1_0_client_without_headers_still_syncs() {
        let addr = start_server();
        let resp = ureq::post(&format!("http://{addr}/sync"))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(SyncRequest {
                cursor: 0,
                changes: vec![],
            })
            .unwrap();
        let protocol = resp.headers().get(PROTOCOL_HEADER).unwrap();
        assert_eq!(protocol.to_str().unwrap(), PROTOCOL.to_string());
    }

    #[test]
    fn a_0_1_0_server_without_headers() {
        let cfg = cfg(start_with(server::router_like_0_1_0));
        let mut app = App::open_in_memory().unwrap();
        // Same protocol: works, and the missing version reads as 0.1.0.
        let stats = sync(&mut app, &cfg).unwrap();
        assert_eq!(stats.server_version, None);
        // A newer protocol can't be refused by that server, so the client
        // notices from the reply instead.
        let err = sync_as(&mut app, &cfg, PROTOCOL + 1).unwrap_err();
        assert!(err.to_string().contains("todont 0.1.0"), "{err}");
    }

    #[test]
    fn version_notes() {
        let stats = |v: &str| Stats {
            server_version: Some(v.into()),
            ..Default::default()
        };
        assert_eq!(stats(VERSION).version_note(), None);
        let newer = stats("999.0.0").version_note().unwrap();
        assert!(newer.contains("cargo install todont"), "{newer}");
        let older = stats("0.0.1").version_note().unwrap();
        assert!(older.contains("update the server"), "{older}");
    }
}
