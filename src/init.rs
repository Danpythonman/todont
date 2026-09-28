//! `td init`: first-run setup. Asks a few questions (Enter takes the
//! default shown in brackets), writes `config.toml`, and checks it works.
//!
//!   td init                  a client: where's the server, what's the token
//!   td init --server         the server: token, ntfy topic, etc. (generated)
//!
//! With `--yes`, or without a terminal, every default is taken as-is.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{Config, NtfyConfig, SyncConfig};
use crate::core::App;
use crate::{notify, sync};

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub server: bool,
    pub url: Option<String>,
    pub token: Option<String>,
    pub yes: bool,
    pub force: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error(
        "{path} already has a [{section}] section; pass --force to replace it"
    )]
    Exists {
        path: PathBuf,
        section: &'static str,
    },
    #[error("{0}")]
    Invalid(String),
}

// ---- Prompting -----------------------------------------------------------

/// Asks questions, or when not interactive, answers them with the defaults
/// (still printing them, so a scripted run shows what it chose).
pub struct Prompter<R, W> {
    input: R,
    out: W,
    interactive: bool,
}

type Check<'a> = &'a dyn Fn(&str) -> Result<(), String>;

impl<R: BufRead, W: Write> Prompter<R, W> {
    pub fn new(input: R, out: W, interactive: bool) -> Self {
        Prompter {
            input,
            out,
            interactive,
        }
    }

    fn line(&mut self) -> Result<String, InitError> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Err(InitError::Invalid(
                "input ended; setup cancelled".into(),
            ));
        }
        Ok(line.trim().to_string())
    }

    /// Asks until `check` accepts the answer; Enter takes `default`.
    pub fn ask(
        &mut self,
        question: &str,
        default: &str,
        check: Check,
    ) -> Result<String, InitError> {
        if !self.interactive {
            writeln!(self.out, "{question}: {default}")?;
            check(default)
                .map_err(|e| InitError::Invalid(format!("{question}: {e}")))?;
            return Ok(default.to_string());
        }
        loop {
            if default.is_empty() {
                write!(self.out, "{question}: ")?;
            } else {
                write!(self.out, "{question} [{default}]: ")?;
            }
            self.out.flush()?;
            let line = self.line()?;
            let answer = if line.is_empty() { default } else { &line };
            match check(answer) {
                Ok(()) => return Ok(answer.to_string()),
                Err(e) => writeln!(self.out, "  {e}")?,
            }
        }
    }

    pub fn confirm(
        &mut self,
        question: &str,
        default: bool,
    ) -> Result<bool, InitError> {
        let hint = if default { "Y/n" } else { "y/N" };
        if !self.interactive {
            let answer = if default { "yes" } else { "no" };
            writeln!(self.out, "{question} [{hint}]: {answer}")?;
            return Ok(default);
        }
        loop {
            write!(self.out, "{question} [{hint}]: ")?;
            self.out.flush()?;
            match self.line()?.to_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => writeln!(self.out, "  please answer y or n")?,
            }
        }
    }

    fn say(&mut self, text: &str) -> Result<(), InitError> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }
}

// ---- Checks --------------------------------------------------------------

fn any(_: &str) -> Result<(), String> {
    Ok(())
}

fn http_url(s: &str) -> Result<(), String> {
    if s.starts_with("http://") || s.starts_with("https://") {
        Ok(())
    } else {
        Err("must start with http:// or https://".into())
    }
}

fn token(s: &str) -> Result<(), String> {
    match s.len() {
        0 => {
            Err("required; it's in the server's config (or pass --token)"
                .into())
        }
        1..16 => Err("too short; use at least 16 characters".into()),
        _ if s.chars().any(char::is_whitespace) => {
            Err("must not contain spaces".into())
        }
        _ => Ok(()),
    }
}

/// "127.0.0.1:8787", "0.0.0.0:8787", "localhost:8787", "[::]:8787".
fn listen_addr(s: &str) -> Result<(), String> {
    match s.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && port.parse::<u16>().is_ok() =>
        {
            Ok(())
        }
        _ => Err("expected host:port, like 127.0.0.1:8787".into()),
    }
}

fn time_zone(s: &str) -> Result<(), String> {
    jiff::tz::TimeZone::get(s)
        .map(|_| ())
        .map_err(|_| "unknown time zone; try e.g. America/Toronto".into())
}

fn ntfy_topic(s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("use 1-64 letters, digits, - or _".into())
    }
}

fn minutes(s: &str) -> Result<(), String> {
    s.parse::<u32>()
        .map(|_| ())
        .map_err(|_| "expected a whole number of minutes".into())
}

// ---- Rendering -----------------------------------------------------------

/// Everything `td init --server` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSetup {
    pub listen: String,
    pub db: PathBuf,
    pub token: String,
    pub timezone: String,
    pub ntfy: Option<NtfySetup>,
    /// Also sync this machine's own `td` with the server.
    pub local_client: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfySetup {
    pub url: String,
    pub topic: String,
    /// Kept from an existing config; `init` never asks for one.
    pub token: Option<String>,
    pub lead_minutes: u32,
    pub nag_minutes: u32,
}

/// A TOML string literal, escaped.
fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn render_sync(url: &str, token: &str) -> String {
    format!(
        "[sync]\n\
         server = {}\n\
         token = {}\n",
        q(url),
        q(token)
    )
}

/// The `[server]` and (if enabled) `[ntfy]` sections. Comments go inside
/// their section so `merge` keeps them with it.
fn render_server(s: &ServerSetup) -> String {
    let mut out = format!(
        "[server]\n\
         listen = {}\n\
         db = {}\n\
         # Every device's [sync] token must match this.\n\
         token = {}\n\
         # Time zone for times in notifications.\n\
         timezone = {}\n",
        q(&s.listen),
        q(&s.db.to_string_lossy()),
        q(&s.token),
        q(&s.timezone),
    );
    if let Some(n) = &s.ntfy {
        out += &format!(
            "\n[ntfy]\n\
             # On a public server, anyone who knows the topic can read it.\n\
             url = {}\n\
             topic = {}\n\
             # Remind this many minutes before the due time.\n\
             lead_minutes = {}\n\
             # Re-notify overdue tasks this often; 0 turns it off.\n\
             nag_minutes = {}\n",
            q(&n.url),
            q(&n.topic),
            n.lead_minutes,
            n.nag_minutes,
        );
        if let Some(t) = &n.token {
            out += &format!("token = {}\n", q(t));
        }
    }
    out
}

/// `existing` with the `drop` sections removed and `new` appended. Every
/// other line, comments included, is kept as written.
fn merge(existing: &str, drop: &[&str], new: &str) -> String {
    let mut kept = String::new();
    let mut keep = true;
    for line in existing.lines() {
        let t = line.trim();
        if let Some(name) =
            t.strip_prefix('[').and_then(|t| t.strip_suffix(']'))
        {
            keep = !drop.contains(&name.trim());
        }
        if keep {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    match kept.trim_end() {
        "" => new.to_string(),
        kept => format!("{kept}\n\n{new}"),
    }
}

/// How this machine reaches a server listening on `listen`.
fn local_url(listen: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or("8787");
    format!("http://127.0.0.1:{port}")
}

// ---- Flows ---------------------------------------------------------------

/// Random hex from the OS RNG (via UUID v4), `2 * 16` chars per UUID.
fn random_hex(uuids: usize) -> String {
    (0..uuids)
        .map(|_| uuid::Uuid::new_v4().simple().to_string())
        .collect()
}

/// The config file as it is before `init` touches it.
struct Existing {
    text: String,
    config: Config,
}

impl Existing {
    fn load(path: &Path, force: bool) -> Result<Self, InitError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        match Config::load(path) {
            Ok(config) => Ok(Existing { text, config }),
            // Can't merge into a file we can't read; start over if told to.
            Err(_) if force => Ok(Existing {
                text: String::new(),
                config: Config::default(),
            }),
            Err(e) => Err(InitError::Invalid(format!(
                "{e}\nFix it, or pass --force to start a new config."
            ))),
        }
    }

    fn has(&self, section: &str) -> bool {
        match section {
            "sync" => self.config.sync.is_some(),
            "server" => self.config.server.is_some(),
            "ntfy" => self.config.ntfy.is_some(),
            _ => false,
        }
    }
}

/// Asks before replacing a section that's already there.
fn confirm_replace<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    path: &Path,
    existing: &Existing,
    section: &'static str,
    force: bool,
) -> Result<(), InitError> {
    if force || !existing.has(section) {
        return Ok(());
    }
    let question = format!(
        "{} already has a [{section}] section. Replace it? \
         (its current values are the defaults)",
        path.display()
    );
    if p.interactive && p.confirm(&question, false)? {
        return Ok(());
    }
    Err(InitError::Exists {
        path: path.to_path_buf(),
        section,
    })
}

fn write_config(path: &Path, text: &str) -> Result<(), InitError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // It holds the token.
        file.mode(0o600);
        let f = file.open(path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        (&f).write_all(text.as_bytes())?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        file.open(path)?.write_all(text.as_bytes())?;
        Ok(())
    }
}

/// `td init`: set this machine up as a sync client.
pub fn init_client<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    path: &Path,
    opts: &Options,
    check: impl FnOnce(&SyncConfig) -> Result<String, String>,
) -> Result<(), InitError> {
    let existing = Existing::load(path, opts.force)?;
    confirm_replace(p, path, &existing, "sync", opts.force)?;
    let current = existing.config.sync.clone();
    p.say("Setting up td to sync with your todont server.")?;
    p.say(
        "(Run `td init --server` on the server first; it prints the token.)\n",
    )?;

    let server = match &opts.url {
        Some(url) => {
            http_url(url)
                .map_err(|e| InitError::Invalid(format!("--url: {e}")))?;
            url.clone()
        }
        None => {
            let default = current
                .as_ref()
                .map_or("http://127.0.0.1:8787", |c| c.server.as_str());
            p.ask("Sync server URL", default, &http_url)?
        }
    };
    let token_value = match &opts.token {
        Some(t) => {
            token(t)
                .map_err(|e| InitError::Invalid(format!("--token: {e}")))?;
            t.clone()
        }
        None => {
            let default = current.as_ref().map_or("", |c| c.token.as_str());
            p.ask("Sync token", default, &token)?
        }
    };
    let text = render_sync(&server, &token_value);
    write_config(path, &merge(&existing.text, &["sync"], &text))?;
    p.say(&format!("\nWrote {}", path.display()))?;

    let cfg = SyncConfig {
        server,
        token: token_value,
    };
    p.say("Checking the connection...")?;
    match check(&cfg) {
        Ok(msg) => p.say(&format!("  ok: {msg}"))?,
        Err(e) => {
            p.say(&format!("  couldn't sync yet: {e}"))?;
            p.say(
                "  The config is saved; run `td sync` once the server is up.",
            )?;
        }
    }
    Ok(())
}

/// `td init --server`: generate the server's config.
pub fn init_server<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    path: &Path,
    local_db: &Path,
    opts: &Options,
    send_test: impl FnOnce(&NtfyConfig) -> Result<(), String>,
) -> Result<ServerSetup, InitError> {
    let existing = Existing::load(path, opts.force)?;
    confirm_replace(p, path, &existing, "server", opts.force)?;
    confirm_replace(p, path, &existing, "ntfy", opts.force)?;
    let current = existing.config.server.clone();
    let current_ntfy = existing.config.ntfy.clone();
    p.say(
        "Setting up the todont server. Press Enter to take the [default].\n",
    )?;

    let default_listen = current
        .as_ref()
        .map_or("127.0.0.1:8787", |c| c.listen.as_str());
    let listen = p.ask("Listen on", default_listen, &listen_addr)?;
    let default_db = match &current {
        Some(c) => c.db.clone(),
        // A system-wide config implies a system service.
        None if path.starts_with("/etc") => {
            PathBuf::from("/var/lib/todont/server.db")
        }
        // Next to this machine's own database (respects --db).
        None => local_db.with_file_name("server.db"),
    };
    let db = p.ask("Server database", &default_db.to_string_lossy(), &any)?;
    let token_value = match &opts.token {
        Some(t) => {
            token(t)
                .map_err(|e| InitError::Invalid(format!("--token: {e}")))?;
            t.clone()
        }
        None => {
            // Keep the current token so existing devices keep working;
            // a client-only config's token is the server's too.
            let default = current
                .as_ref()
                .map(|c| c.token.clone())
                .or_else(|| {
                    existing.config.sync.as_ref().map(|c| c.token.clone())
                })
                .unwrap_or_else(|| random_hex(2));
            p.ask("Sync token", &default, &token)?
        }
    };
    let default_tz = current
        .as_ref()
        .and_then(|c| c.timezone.clone())
        .or_else(|| jiff::tz::TimeZone::system().iana_name().map(String::from))
        .unwrap_or_else(|| "UTC".into());
    let timezone = p.ask("Time zone", &default_tz, &time_zone)?;

    // Default on for a new setup; otherwise keep whatever was chosen.
    let ntfy_default = current_ntfy.is_some() || current.is_none();
    let ntfy = if p
        .confirm("Send reminders to your phone with ntfy?", ntfy_default)?
    {
        let c = current_ntfy.as_ref();
        let url = p.ask(
            "ntfy server",
            c.map_or("https://ntfy.sh", |c| c.url.as_str()),
            &http_url,
        )?;
        // Keep the topic, so the phone's subscription keeps working.
        let default_topic = c.map_or_else(
            || format!("todont-{}", &random_hex(1)[..16]),
            |c| c.topic.clone(),
        );
        let topic =
            p.ask("ntfy topic (keep it secret)", &default_topic, &ntfy_topic)?;
        let lead = p.ask(
            "Remind how many minutes before due",
            &c.map_or(0, |c| c.lead_minutes).to_string(),
            &minutes,
        )?;
        let nag = p.ask(
            "Re-notify overdue tasks every N minutes (0 = never)",
            &c.map_or(120, |c| c.nag_minutes).to_string(),
            &minutes,
        )?;
        Some(NtfySetup {
            url,
            topic,
            token: c.and_then(|c| c.token.clone()),
            lead_minutes: lead.parse().expect("checked"),
            nag_minutes: nag.parse().expect("checked"),
        })
    } else {
        None
    };
    // A system service's config is no place for a user's sync settings.
    let local_client = p.confirm(
        "Also use td on this machine, synced with this server?",
        !path.starts_with("/etc"),
    )?;

    let setup = ServerSetup {
        listen,
        db: PathBuf::from(db),
        token: token_value,
        timezone,
        ntfy,
        local_client,
    };
    let mut drop = vec!["server", "ntfy"];
    let mut text = render_server(&setup);
    if local_client {
        drop.push("sync");
        let sync = render_sync(&local_url(&setup.listen), &setup.token);
        text = format!("{sync}\n{text}");
    }
    write_config(path, &merge(&existing.text, &drop, &text))?;
    p.say(&format!("\nWrote {}", path.display()))?;

    if let Some(n) = &setup.ntfy {
        p.say(&format!(
            "\nOn your phone: install the ntfy app and subscribe to \
             \"{}\" on {}.",
            n.topic, n.url
        ))?;
        // Scripted runs don't send anything unless asked interactively.
        let interactive = p.interactive;
        if p.confirm("Send a test notification now?", interactive)? {
            let cfg = NtfyConfig {
                topic: n.topic.clone(),
                url: n.url.clone(),
                token: n.token.clone(),
                lead_minutes: n.lead_minutes,
                nag_minutes: n.nag_minutes,
            };
            match send_test(&cfg) {
                Ok(()) => {
                    p.say("  sent; it should arrive in a few seconds")?
                }
                Err(e) => p.say(&format!("  sending failed: {e}"))?,
            }
        }
    }

    next_steps(p, &setup)?;
    Ok(setup)
}

fn next_steps<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    s: &ServerSetup,
) -> Result<(), InitError> {
    let port = s.listen.rsplit(':').next().unwrap_or("8787");
    let loopback = ["127.", "[::1]", "localhost:"]
        .iter()
        .any(|p| s.listen.starts_with(p));
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "this-host".into());
    let url = if loopback {
        "https://<your-url>".to_string()
    } else {
        format!("http://{host}:{port}")
    };
    p.say("\nNext:")?;
    p.say("  1. Start it:  td serve   (or install deploy/todont.service)")?;
    if loopback {
        p.say(&format!(
            "  2. It only listens locally. To reach it from other devices, \
             put HTTPS in front,\n     e.g. `tailscale serve --bg {port}`, \
             and use that URL below."
        ))?;
    } else {
        p.say("  2. It's plain HTTP; only expose it on a network you trust.")?;
    }
    p.say(&format!(
        "  3. On each other device:\n     td init --url {url} --token {}",
        s.token
    ))?;
    Ok(())
}

/// Entry point for `td init`. Returns the process exit code.
pub fn run(opts: &Options, config_path: &Path, db_path: &Path) -> i32 {
    let interactive = !opts.yes && io::stdin().is_terminal();
    let stdin = io::stdin();
    let mut p = Prompter::new(stdin.lock(), io::stdout(), interactive);
    let result = if opts.server {
        init_server(&mut p, config_path, db_path, opts, |cfg| {
            notify::Sender::new(cfg).send(&notify::test_message(&cfg.topic))
        })
        .map(|_| ())
    } else {
        init_client(&mut p, config_path, opts, |cfg| {
            let mut app = App::open(db_path).map_err(|e| e.to_string())?;
            sync::sync(&mut app, cfg)
                .map(|s| {
                    format!(
                        "synced ({} pushed, {} pulled)",
                        s.pushed, s.pulled
                    )
                })
                .map_err(|e| e.to_string())
        })
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("td init: {e}");
            match e {
                InitError::Exists { .. } => 73, // EX_CANTCREAT
                InitError::Invalid(_) => 2,
                InitError::Io(_) => 74, // EX_IOERR
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::config::Config;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("todont-init-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    fn prompter(input: &str) -> Prompter<Cursor<Vec<u8>>, Vec<u8>> {
        Prompter::new(Cursor::new(input.as_bytes().to_vec()), Vec::new(), true)
    }

    fn output(p: &Prompter<Cursor<Vec<u8>>, Vec<u8>>) -> String {
        String::from_utf8_lossy(&p.out).into_owned()
    }

    #[test]
    fn ask_takes_default_and_reasks_on_bad_input() {
        let mut p = prompter("\nnope\n9000\n");
        assert_eq!(p.ask("N", "5", &minutes).unwrap(), "5");
        assert_eq!(p.ask("N", "5", &minutes).unwrap(), "9000");
        assert!(output(&p).contains("expected a whole number"));
    }

    #[test]
    fn non_interactive_takes_defaults_but_still_validates() {
        let mut p = Prompter::new(Cursor::new(Vec::new()), Vec::new(), false);
        assert_eq!(p.ask("Port", "8787", &minutes).unwrap(), "8787");
        assert!(p.confirm("Ok?", true).unwrap());
        assert!(matches!(
            p.ask("Sync token", "", &token),
            Err(InitError::Invalid(_))
        ));
    }

    #[test]
    fn server_defaults_make_a_valid_config() {
        let path = temp("server");
        let mut p = Prompter::new(Cursor::new(Vec::new()), Vec::new(), false);
        let mut sent = false;
        let setup = init_server(
            &mut p,
            &path,
            &path.with_file_name("td.db"),
            &Options::default(),
            |_| {
                sent = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(!sent, "scripted runs must not send by default");

        let c = Config::load(&path).unwrap();
        let server = c.server.unwrap();
        assert_eq!(server.listen, "127.0.0.1:8787");
        assert_eq!(server.db, path.with_file_name("server.db"));
        assert_eq!(server.token.len(), 64);
        let ntfy = c.ntfy.unwrap();
        assert!(ntfy.topic.starts_with("todont-"));
        assert_eq!((ntfy.lead_minutes, ntfy.nag_minutes), (0, 120));
        // This machine is set up as a client of itself.
        let sync = c.sync.unwrap();
        assert_eq!(sync.server, "http://127.0.0.1:8787");
        assert_eq!(sync.token, setup.token);
        assert!(output(&p).contains(&format!("--token {}", setup.token)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn server_answers_are_used() {
        let path = temp("answers");
        let answers = [
            "0.0.0.0:9000",
            "/tmp/x.db",
            "my-very-own-long-token",
            "UTC",
            "y",
            "",
            "my_topic",
            "15",
            "0",
            "n",
            "y", // send test
        ]
        .join("\n");
        let mut p = prompter(&answers);
        let mut sent_to = None;
        init_server(
            &mut p,
            &path,
            &path.with_file_name("td.db"),
            &Options::default(),
            |cfg| {
                sent_to = Some(cfg.topic.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(sent_to.as_deref(), Some("my_topic"));
        let c = Config::load(&path).unwrap();
        assert!(c.sync.is_none());
        assert_eq!(c.server.unwrap().listen, "0.0.0.0:9000");
        let ntfy = c.ntfy.unwrap();
        assert_eq!((ntfy.lead_minutes, ntfy.nag_minutes), (15, 0));
        assert!(output(&p).contains("--url http://"));
    }

    #[test]
    fn client_from_flags_checks_the_connection() {
        let path = temp("client");
        let mut p = Prompter::new(Cursor::new(Vec::new()), Vec::new(), false);
        let opts = Options {
            url: Some("https://todo.example.net".into()),
            token: Some("0123456789abcdef0123".into()),
            ..Default::default()
        };
        let mut checked = None;
        init_client(&mut p, &path, &opts, |cfg| {
            checked = Some(cfg.server.clone());
            Err("connection refused".into())
        })
        .unwrap();
        assert_eq!(checked.as_deref(), Some("https://todo.example.net"));
        assert!(output(&p).contains("run `td sync` once the server is up"));
        let sync = Config::load(&path).unwrap().sync.unwrap();
        assert_eq!(sync.token, "0123456789abcdef0123");
    }

    const TOKEN: &str = "0123456789abcdef0123";

    fn scripted() -> Prompter<Cursor<Vec<u8>>, Vec<u8>> {
        Prompter::new(Cursor::new(Vec::new()), Vec::new(), false)
    }

    fn client_opts(url: &str) -> Options {
        Options {
            url: Some(url.into()),
            token: Some(TOKEN.into()),
            ..Default::default()
        }
    }

    fn ok(_: &SyncConfig) -> Result<String, String> {
        Ok(String::new())
    }

    fn server(path: &Path, opts: &Options) -> ServerSetup {
        let db = path.with_file_name("td.db");
        init_server(&mut scripted(), path, &db, opts, |_| Ok(())).unwrap()
    }

    #[test]
    fn existing_sync_needs_force_or_a_yes() {
        let path = temp("exists");
        let mine = format!(
            "# mine\n[sync]\nserver = \"http://old\"\ntoken = \"{TOKEN}\"\n"
        );
        write_config(&path, &mine).unwrap();
        let opts = Options {
            token: Some(TOKEN.into()),
            ..Default::default()
        };

        let r = init_client(&mut scripted(), &path, &opts, ok);
        assert!(matches!(
            r,
            Err(InitError::Exists {
                section: "sync",
                ..
            })
        ));
        let r = init_client(&mut prompter("n\n"), &path, &opts, ok);
        assert!(matches!(r, Err(InitError::Exists { .. })));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);

        // Yes, then Enter: the current URL is the default.
        init_client(&mut prompter("y\n\n"), &path, &opts, ok).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        assert_eq!(text.lines().filter(|l| *l == "[sync]").count(), 1);
        let sync = Config::load(&path).unwrap().sync.unwrap();
        assert_eq!(sync.server, "http://old");
    }

    #[test]
    fn server_init_after_client_init_keeps_the_token() {
        let path = temp("client-then-server");
        let opts = client_opts("http://127.0.0.1:8787");
        init_client(&mut scripted(), &path, &opts, ok).unwrap();
        let setup = server(&path, &Options::default());

        let c = Config::load(&path).unwrap();
        assert_eq!(setup.token, TOKEN);
        assert_eq!(c.server.unwrap().token, TOKEN);
        assert_eq!(c.sync.unwrap().token, TOKEN);
        assert!(c.ntfy.is_some());
    }

    #[test]
    fn client_init_after_server_init_keeps_the_server() {
        let path = temp("server-then-client");
        let setup = server(&path, &Options::default());
        let opts = Options {
            force: true,
            ..client_opts("https://elsewhere")
        };
        init_client(&mut scripted(), &path, &opts, ok).unwrap();

        let c = Config::load(&path).unwrap();
        assert_eq!(c.sync.unwrap().server, "https://elsewhere");
        assert_eq!(c.server.unwrap().token, setup.token);
        assert!(c.ntfy.is_some());
    }

    #[test]
    fn rerunning_server_init_keeps_token_and_topic() {
        let path = temp("rerun");
        let first = server(&path, &Options::default());
        let r = init_server(
            &mut scripted(),
            &path,
            &path,
            &Options::default(),
            |_| Ok(()),
        );
        assert!(matches!(
            r,
            Err(InitError::Exists {
                section: "server",
                ..
            })
        ));
        let force = Options {
            force: true,
            ..Default::default()
        };
        let second = server(&path, &force);
        assert_eq!(second.token, first.token);
        assert_eq!(second.ntfy.map(|n| n.topic), first.ntfy.map(|n| n.topic));
        let text = std::fs::read_to_string(&path).unwrap();
        for section in ["[sync]", "[server]", "[ntfy]"] {
            let headers = text.lines().filter(|l| l.trim() == section);
            assert_eq!(headers.count(), 1, "{text}");
        }
    }

    #[test]
    fn merge_only_touches_named_sections() {
        let old = "# top\n\n[sync]\n# about sync\nserver = \"a\"\n\n\
                   [server]\n# keep me\nlisten = \"x\"\n";
        let new = merge(old, &["sync"], "[sync]\nserver = \"b\"\n");
        assert_eq!(
            new,
            "# top\n\n[server]\n# keep me\nlisten = \"x\"\n\n\
             [sync]\nserver = \"b\"\n"
        );
        assert_eq!(merge("", &["sync"], "[sync]\n"), "[sync]\n");
    }

    #[test]
    fn broken_config_is_not_clobbered_without_force() {
        let path = temp("broken");
        write_config(&path, "this is not toml [").unwrap();
        let opts = client_opts("http://x");
        let r = init_client(&mut scripted(), &path, &opts, ok);
        assert!(matches!(r, Err(InitError::Invalid(_))));
        let force = Options {
            force: true,
            ..opts
        };
        init_client(&mut scripted(), &path, &force, ok).unwrap();
        assert!(Config::load(&path).unwrap().sync.is_some());
    }

    #[test]
    fn listen_addresses() {
        for ok in ["127.0.0.1:8787", "0.0.0.0:80", "localhost:8787", "[::]:1"]
        {
            assert!(listen_addr(ok).is_ok(), "{ok}");
        }
        for bad in ["localhost", ":8787", "host:99999", "host:x"] {
            assert!(listen_addr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn strings_are_escaped() {
        let text = render_sync("http://x", "a\"b\\c");
        let c: Config = toml::from_str(&text).unwrap();
        assert_eq!(c.sync.unwrap().token, "a\"b\\c");
    }
}
