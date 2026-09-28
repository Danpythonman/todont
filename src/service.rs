//! `td service install|uninstall`: runs `td serve` as a systemd *user*
//! service, so it starts with your session (or at boot, with lingering)
//! and restarts if it crashes. Linux only.
//!
//! For a system-wide service instead, see `deploy/todont.service`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ConfigError};

const UNIT: &str = "todont.service";

/// First line of every unit this writes, so `td` never overwrites or
/// deletes a unit file someone else wrote.
const MARKER: &str = "# Written by `td service install`.";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("`systemctl --user {args}` failed: {msg}")]
    Systemctl { args: String, msg: String },
    #[error("{0}")]
    Unsupported(String),
}

/// `$XDG_CONFIG_HOME/systemd/user/todont.service`.
fn unit_path() -> Result<PathBuf, ServiceError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })
        .ok_or_else(|| ServiceError::Unsupported("$HOME is not set".into()))?;
    Ok(base.join("systemd/user").join(UNIT))
}

/// A path as one quoted `ExecStart` argument, with systemd's `%`
/// specifiers and `$` variables escaped.
fn quote(path: &Path) -> Result<String, ServiceError> {
    let s = path.to_str().ok_or_else(|| {
        ServiceError::Unsupported(format!("{} isn't UTF-8", path.display()))
    })?;
    if s.contains(['"', '\\', '\n']) {
        return Err(ServiceError::Unsupported(format!(
            "can't put {s:?} in a unit file"
        )));
    }
    Ok(format!("\"{}\"", s.replace('%', "%%").replace('$', "$$")))
}

pub fn unit(exe: &Path, config: &Path) -> Result<String, ServiceError> {
    Ok(format!(
        "{MARKER} Remove it with `td service uninstall`.\n\
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

fn systemctl(args: &[&str]) -> Result<String, ServiceError> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| ServiceError::Systemctl {
            args: args.join(" "),
            msg: e.to_string(),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(ServiceError::Systemctl {
            args: args.join(" "),
            msg: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
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

pub fn install(config: &Config) -> Result<(), ServiceError> {
    linux_only()?;
    config.server()?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let config_path = config.path.canonicalize()?;
    let path = unit_path()?;
    let existed = check_ours(&path)?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, unit(&exe, &config_path)?)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    // Restart rather than start, so reinstalling picks up changes.
    systemctl(&["restart", UNIT])?;

    println!(
        "{} {}",
        if existed { "Updated" } else { "Installed" },
        path.display()
    );
    println!(
        "  runs:   {} serve --config {}",
        exe.display(),
        config_path.display()
    );
    if exe.components().any(|c| c.as_os_str() == "target") {
        println!(
            "  note:   that's a build directory; for a lasting setup, \
             `cargo install todont` and rerun this"
        );
    }

    // Give it a moment to either come up or fall over (e.g. port in use).
    std::thread::sleep(std::time::Duration::from_millis(800));
    match systemctl(&["is-active", UNIT]) {
        Ok(_) => println!("\ntd serve is running."),
        Err(_) => {
            println!("\ntd serve didn't stay up. See why with:");
            println!("  journalctl --user -u todont -n 20");
        }
    }
    println!("  logs:          journalctl --user -u todont -f");
    println!("  status:        systemctl --user status todont");
    println!("  after updates: systemctl --user restart todont");

    let user = std::env::var("USER").unwrap_or_default();
    let linger = Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if linger != "yes" {
        println!(
            "\nIt stops when you log out. To keep it running, and start it \
             at boot:\n  loginctl enable-linger"
        );
    }
    Ok(())
}

pub fn uninstall() -> Result<(), ServiceError> {
    linux_only()?;
    let path = unit_path()?;
    if !check_ours(&path)? {
        println!("No todont service installed ({}).", path.display());
        return Ok(());
    }
    // Fine if it's already stopped or disabled.
    let _ = systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(&path)?;
    systemctl(&["daemon-reload"])?;
    println!("Stopped and removed {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_quotes_and_escapes_paths() {
        let u = unit(
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
    fn unquotable_paths_are_refused() {
        assert!(quote(Path::new("/a\"b")).is_err());
        assert!(quote(Path::new("/a\nb")).is_err());
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
        std::fs::write(&path, format!("{MARKER}\n")).unwrap();
        assert!(check_ours(&path).unwrap());
    }
}
