//! `td service install|uninstall [--system]`: runs `td serve` under
//! systemd so it restarts if it crashes. Linux only.
//!
//! - User service (default): runs as you, with your config, from the `td`
//!   you ran; starts with your session, or at boot with lingering.
//! - System service (`--system`, as root): copies `td` to /usr/local/bin,
//!   reads /etc/todont/config.toml, runs as a throwaway system user that
//!   can only write /var/lib/todont, and starts at boot. For always-on
//!   boxes like a Raspberry Pi.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ConfigError};

const UNIT: &str = "todont.service";

/// Every unit this writes starts with this, so `td` never overwrites or
/// deletes a unit file someone else wrote.
const MARKER: &str = "# Written by `td service install";

/// Where `--system` puts things.
pub const SYSTEM_CONFIG: &str = "/etc/todont/config.toml";
const SYSTEM_BIN: &str = "/usr/local/bin/td";
const SYSTEM_STATE: &str = "/var/lib/todont";
const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("`systemctl {args}` failed: {msg}")]
    Systemctl { args: String, msg: String },
    #[error("{0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn unit_path(self) -> Result<PathBuf, ServiceError> {
        match self {
            Scope::System => Ok(Path::new(SYSTEM_UNIT_DIR).join(UNIT)),
            Scope::User => {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join(".config"))
                    })
                    .ok_or_else(|| {
                        ServiceError::Unsupported("$HOME is not set".into())
                    })?;
                Ok(base.join("systemd/user").join(UNIT))
            }
        }
    }

    fn systemctl(self, args: &[&str]) -> Result<String, ServiceError> {
        let mut cmd = Command::new("systemctl");
        if self == Scope::User {
            cmd.arg("--user");
        }
        let shown = format!(
            "{}{}",
            if self == Scope::User { "--user " } else { "" },
            args.join(" ")
        );
        let out =
            cmd.args(args)
                .output()
                .map_err(|e| ServiceError::Systemctl {
                    args: shown.clone(),
                    msg: e.to_string(),
                })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(ServiceError::Systemctl {
                args: shown,
                msg: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// How to see the service's logs and status.
    fn journal(self) -> &'static str {
        match self {
            Scope::User => "journalctl --user -u todont",
            Scope::System => "journalctl -u todont",
        }
    }

    fn status(self) -> &'static str {
        match self {
            Scope::User => "systemctl --user status todont",
            Scope::System => "systemctl status todont",
        }
    }
}

/// A path as one quoted `ExecStart` argument, with systemd's `%`
/// specifiers and `$` variables escaped.
fn quote(path: &Path) -> Result<String, ServiceError> {
    let s = plain(path)?;
    Ok(format!("\"{}\"", s.replace('%', "%%").replace('$', "$$")))
}

/// A path systemd can take as-is (no quoting, escaping or spaces), for
/// settings like `LoadCredential=` that don't accept quotes.
fn plain(path: &Path) -> Result<&str, ServiceError> {
    let s = path.to_str().ok_or_else(|| {
        ServiceError::Unsupported(format!("{} isn't UTF-8", path.display()))
    })?;
    if s.contains(['"', '\\', '\n']) {
        return Err(ServiceError::Unsupported(format!(
            "can't put {s:?} in a unit file"
        )));
    }
    Ok(s)
}

pub fn user_unit(exe: &Path, config: &Path) -> Result<String, ServiceError> {
    Ok(format!(
        "{MARKER}`. Remove it with `td service uninstall`.\n\
         [Unit]\n\
         Description=todont sync server and ntfy notifier\n\
         Documentation=https://github.com/Danpythonman/todont\n\
         \n\
         [Service]\n\
         ExecStart={} serve --config {}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(exe)?,
        quote(config)?,
    ))
}

pub fn system_unit(config: &Path) -> Result<String, ServiceError> {
    let config = plain(config)?;
    if config.contains(char::is_whitespace) {
        return Err(ServiceError::Unsupported(format!(
            "the system service can't read a config path with spaces \
             ({config:?}); use {SYSTEM_CONFIG}"
        )));
    }
    Ok(format!(
        "{MARKER} --system`. Remove it with `td service uninstall --system`.\n\
         [Unit]\n\
         Description=todont sync server and ntfy notifier\n\
         Documentation=https://github.com/Danpythonman/todont\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={SYSTEM_BIN} serve \
         --config ${{CREDENTIALS_DIRECTORY}}/config.toml\n\
         # Hands the root-only config (it holds the token) to the service.\n\
         LoadCredential=config.toml:{config}\n\
         # A throwaway system user that can only write {SYSTEM_STATE}.\n\
         DynamicUser=yes\n\
         StateDirectory=todont\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         NoNewPrivileges=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         PrivateTmp=yes\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    ))
}

/// Refuses to touch a unit file `td` didn't write.
fn check_ours(path: &Path) -> Result<bool, ServiceError> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.starts_with(MARKER) => Ok(true),
        Ok(_) => Err(ServiceError::Unsupported(format!(
            "{} wasn't written by td; remove or rename it first",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn linux_only() -> Result<(), ServiceError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(ServiceError::Unsupported(
            "`td service` uses systemd, so it's Linux-only; \
             run `td serve` another way on this system"
                .into(),
        ))
    }
}

/// Effective uid from /proc (Linux only, like the rest of this module).
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("Uid:"))?;
            line.split_whitespace().nth(2).map(|uid| uid == "0")
        })
        .unwrap_or(false)
}

fn need_root(what: &str) -> Result<(), ServiceError> {
    if is_root() {
        return Ok(());
    }
    Err(ServiceError::Unsupported(format!(
        "{what} needs root; run:\n  sudo \"$(command -v td)\" {what}"
    )))
}

/// The system service can only write under /var/lib/todont.
fn check_system_db(config: &Config) -> Result<(), ServiceError> {
    let db = &config.server()?.db;
    if db.starts_with(SYSTEM_STATE) {
        return Ok(());
    }
    Err(ServiceError::Unsupported(format!(
        "the system service can only write under {SYSTEM_STATE}, but the \
         database is {}.\nSet `db = \"{SYSTEM_STATE}/server.db\"` in {}. \
         (Devices re-send their tasks to a new server database \
         automatically.)",
        db.display(),
        config.path.display()
    )))
}

/// Copies the running `td` to /usr/local/bin, replacing it atomically so
/// a running service never sees a half-written binary.
fn install_binary() -> Result<PathBuf, ServiceError> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dest = Path::new(SYSTEM_BIN);
    if exe == dest {
        return Ok(exe);
    }
    let tmp = dest.with_extension("new");
    std::fs::copy(&exe, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&tmp, mode)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(exe)
}

pub fn install(config: &Config, scope: Scope) -> Result<(), ServiceError> {
    linux_only()?;
    if scope == Scope::System {
        need_root("service install --system")?;
    }
    config.server()?;
    let config_path = config.path.canonicalize()?;
    let path = scope.unit_path()?;
    let existed = check_ours(&path)?;

    let (unit, runs) = match scope {
        Scope::User => {
            let exe = std::env::current_exe()?.canonicalize()?;
            if exe.components().any(|c| c.as_os_str() == "target") {
                println!(
                    "note: {} is a build directory; for a lasting setup, \
                     `cargo install todont` and rerun this",
                    exe.display()
                );
            }
            (user_unit(&exe, &config_path)?, exe)
        }
        Scope::System => {
            check_system_db(config)?;
            let unit = system_unit(&config_path)?;
            let from = install_binary()?;
            println!("Copied {} to {SYSTEM_BIN}", from.display());
            (unit, PathBuf::from(SYSTEM_BIN))
        }
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, unit)?;
    scope.systemctl(&["daemon-reload"])?;
    scope.systemctl(&["enable", UNIT])?;
    // Restart rather than start, so reinstalling picks up a new binary.
    scope.systemctl(&["restart", UNIT])?;

    let verb = if existed { "Updated" } else { "Installed" };
    println!("{verb} {}", path.display());
    println!(
        "  runs:   {} serve --config {}",
        runs.display(),
        config_path.display()
    );

    // Give it a moment to either come up or fall over (e.g. port in use).
    std::thread::sleep(std::time::Duration::from_millis(800));
    match scope.systemctl(&["is-active", UNIT]) {
        Ok(_) => println!("\ntd serve is running."),
        Err(_) => {
            println!("\ntd serve didn't stay up. See why with:");
            println!("  {} -n 20", scope.journal());
        }
    }
    println!("  logs:    {} -f", scope.journal());
    println!("  status:  {}", scope.status());
    match scope {
        Scope::User => {
            println!(
                "  update:  cargo install todont && \
                 systemctl --user restart todont"
            );
            let user = std::env::var("USER").unwrap_or_default();
            let linger = Command::new("loginctl")
                .args(["show-user", &user, "--property=Linger", "--value"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if linger != "yes" {
                println!(
                    "\nIt stops when you log out. To keep it running, and \
                     start it at boot:\n  loginctl enable-linger"
                );
            }
        }
        Scope::System => println!(
            "  update:  cargo install todont && \
             sudo \"$(command -v td)\" service install --system"
        ),
    }
    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<(), ServiceError> {
    linux_only()?;
    if scope == Scope::System {
        need_root("service uninstall --system")?;
    }
    let path = scope.unit_path()?;
    if !check_ours(&path)? {
        println!("No todont service installed ({}).", path.display());
        return Ok(());
    }
    // Fine if it's already stopped or disabled.
    let _ = scope.systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(&path)?;
    scope.systemctl(&["daemon-reload"])?;
    println!("Stopped and removed {}", path.display());
    if scope == Scope::System {
        println!(
            "Left in place: {SYSTEM_BIN}, {SYSTEM_CONFIG} and the data in \
             {SYSTEM_STATE}. Delete them yourself if you're done with them."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit_quotes_and_escapes_paths() {
        let u = user_unit(
            Path::new("/home/me/.cargo/bin/td"),
            Path::new("/home/me/my 100% $config.toml"),
        )
        .unwrap();
        assert!(u.starts_with(MARKER));
        assert!(u.contains(
            "ExecStart=\"/home/me/.cargo/bin/td\" serve \
             --config \"/home/me/my 100%% $$config.toml\"\n"
        ));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn system_unit_is_locked_down() {
        let u = system_unit(Path::new(SYSTEM_CONFIG)).unwrap();
        assert!(u.starts_with(MARKER));
        assert!(u.contains(
            "ExecStart=/usr/local/bin/td serve \
             --config ${CREDENTIALS_DIRECTORY}/config.toml\n"
        ));
        assert!(
            u.contains("LoadCredential=config.toml:/etc/todont/config.toml\n")
        );
        for setting in [
            "DynamicUser=yes",
            "StateDirectory=todont",
            "ProtectSystem=strict",
            "WantedBy=multi-user.target",
        ] {
            assert!(u.contains(setting), "missing {setting}");
        }
        assert!(system_unit(Path::new("/etc/my todont.toml")).is_err());
    }

    #[test]
    fn unquotable_paths_are_refused() {
        assert!(quote(Path::new("/a\"b")).is_err());
        assert!(quote(Path::new("/a\nb")).is_err());
    }

    #[test]
    fn system_db_must_be_writable_by_the_service() {
        let config = |db: &str| -> Config {
            let mut c: Config = toml::from_str(&format!(
                "[server]\ndb = \"{db}\"\ntoken = \"t\"\n"
            ))
            .unwrap();
            c.path = SYSTEM_CONFIG.into();
            c
        };
        assert!(check_system_db(&config("/var/lib/todont/server.db")).is_ok());
        let err =
            check_system_db(&config("/home/pi/.local/share/todont/server.db"))
                .unwrap_err()
                .to_string();
        assert!(err.contains("db = \"/var/lib/todont/server.db\""), "{err}");
    }

    #[test]
    fn foreign_unit_files_are_left_alone() {
        let dir = std::env::temp_dir()
            .join(format!("todont-service-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(UNIT);
        assert!(!check_ours(&path).unwrap());
        std::fs::write(&path, "[Unit]\nDescription=mine\n").unwrap();
        assert!(check_ours(&path).is_err());
        std::fs::write(&path, format!("{MARKER} --system`.\n")).unwrap();
        assert!(check_ours(&path).unwrap());
    }
}
