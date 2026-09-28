//! CLI frontend. Parses argv, calls core, formats to stdout. No domain logic.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use jiff::{Timestamp, Zoned};

use crate::alerts::{Nag, Remind};
use crate::config::Config;
use crate::core::{App, CoreError, Edit, NewTask, Task};
use crate::{due, sync};

#[derive(Parser, Debug)]
#[command(
    name = "td",
    version,
    about = "Todo list; run with no command for the TUI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Command>,

    /// Path to the local database [env: TODONT_DB]
    #[arg(long, global = true, value_name = "FILE")]
    pub db: Option<PathBuf>,

    /// Path to config.toml [env: TODONT_CONFIG]
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Add a task.
    #[command(visible_alias = "a")]
    Add {
        /// Task title; multiple words are joined.
        #[arg(required = true, num_args = 1..)]
        title: Vec<String>,
        /// When it's due, e.g. "tomorrow 5pm", "fri", "sep 30", "+2h".
        #[arg(short, long, value_name = "WHEN")]
        due: Option<String>,
        /// When to notify: default, off, due, or a lead time like 15m.
        #[arg(long, value_name = "WHEN", value_parser = Remind::parse)]
        remind: Option<Remind>,
        /// Re-notify while overdue: default, off, or an interval like 2h.
        #[arg(long, value_name = "EVERY", value_parser = Nag::parse)]
        nag: Option<Nag>,
    },
    /// List tasks.
    #[command(visible_alias = "ls")]
    List {
        /// Include finished tasks.
        #[arg(short, long)]
        all: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Mark tasks done.
    Done {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Mark tasks not done.
    Undo {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Change a task's title or due date.
    Edit {
        id: i64,
        /// New title.
        #[arg(short, long)]
        title: Option<String>,
        /// New due date.
        #[arg(short, long, value_name = "WHEN", conflicts_with = "no_due")]
        due: Option<String>,
        /// Remove the due date.
        #[arg(long)]
        no_due: bool,
        /// When to notify: default, off, due, or a lead time like 15m.
        #[arg(long, value_name = "WHEN", value_parser = Remind::parse)]
        remind: Option<Remind>,
        /// Re-notify while overdue: default, off, or an interval like 2h.
        #[arg(long, value_name = "EVERY", value_parser = Nag::parse)]
        nag: Option<Nag>,
    },
    /// Delete tasks.
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Push and pull changes with the sync server now.
    Sync,
    /// Run the sync server and ntfy notifier (see [server] in the config).
    Serve,
    /// First-time setup: write config.toml for this device or the server.
    Init {
        /// Set up the sync server and ntfy notifier instead of a client.
        #[arg(long)]
        server: bool,
        /// Sync server URL, e.g. https://todo.example.net (client).
        #[arg(long)]
        url: Option<String>,
        /// Sync token; printed by `td init --server`.
        #[arg(long)]
        token: Option<String>,
        /// Take the default for every question not given as a flag.
        #[arg(short, long)]
        yes: bool,
        /// Replace an existing config without asking.
        #[arg(long)]
        force: bool,
    },
}

impl Command {
    /// Whether the command changes tasks, and so should be synced.
    fn mutates(&self) -> bool {
        !matches!(
            self,
            Command::List { .. }
                | Command::Sync
                | Command::Serve
                | Command::Init { .. }
        )
    }
}

impl Cli {
    pub fn db_path(&self) -> PathBuf {
        self.db.clone().unwrap_or_else(App::default_path)
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(Config::default_path)
    }
}

/// Runs one command. Returns the process exit code.
pub fn run(cmd: &Command, app: &mut App, config: &Config) -> i32 {
    if let Command::Sync = cmd {
        return run_sync(app, config);
    }
    match dispatch(cmd, app) {
        Ok(()) => {
            if cmd.mutates() {
                auto_sync(app, config);
            }
            0
        }
        Err(e) => {
            // stderr, not stdout, so `td ls > f` never mixes the two.
            // Each CoreError message already includes its cause.
            eprintln!("td: {e}");
            e.exit_code()
        }
    }
}

fn dispatch(cmd: &Command, app: &mut App) -> Result<(), CoreError> {
    let now = Zoned::now();
    match cmd {
        Command::Add {
            title,
            due,
            remind,
            nag,
        } => {
            let title = title.join(" ");
            let t = app.create(NewTask {
                title: &title,
                due: parse_due(due.as_deref(), &now)?,
                remind: remind.unwrap_or_default(),
                nag: nag.unwrap_or_default(),
            })?;
            match t.due {
                Some(d) => {
                    println!("added {} (due {})", t.id, describe(d, &now))
                }
                None => println!("added {}", t.id),
            }
        }

        Command::List { all, json } => {
            let tasks = app.tasks(*all)?;
            if *json {
                let out = serde_json::to_string(&tasks)
                    .expect("Task always serializes");
                println!("{out}");
            } else {
                print_table(&tasks, &now);
            }
        }

        Command::Done { ids } => {
            for &id in ids {
                let t = app.set_done(id, true)?;
                println!("done {} ({})", t.id, t.title);
            }
        }

        Command::Undo { ids } => {
            for &id in ids {
                let t = app.set_done(id, false)?;
                println!("reopened {} ({})", t.id, t.title);
            }
        }

        Command::Edit {
            id,
            title,
            due,
            no_due,
            remind,
            nag,
        } => {
            let due = if *no_due {
                Some(None)
            } else {
                parse_due(due.as_deref(), &now)?.map(Some)
            };
            let t = app.edit(
                *id,
                Edit {
                    title: title.clone(),
                    due,
                    remind: *remind,
                    nag: *nag,
                },
            )?;
            println!("edited {} ({})", t.id, t.title);
        }

        Command::Remove { ids } => {
            for &id in ids {
                let t = app.remove(id)?;
                println!("removed {} ({})", t.id, t.title);
            }
        }

        Command::Sync | Command::Serve | Command::Init { .. } => {
            unreachable!("handled by caller")
        }
    }
    Ok(())
}

fn run_sync(app: &mut App, config: &Config) -> i32 {
    let Some(cfg) = &config.sync else {
        eprintln!(
            "td: sync is not configured; add a [sync] section to {}",
            config.path.display()
        );
        return 78; // EX_CONFIG
    };
    match sync::sync(app, cfg) {
        Ok(s) => {
            println!("synced: {} pushed, {} pulled", s.pushed, s.pulled);
            0
        }
        Err(e) => {
            eprintln!("td: {e}");
            69 // EX_UNAVAILABLE
        }
    }
}

/// Best-effort sync after a change. Offline is normal, so failure is only
/// a note: the change is safe locally and goes out on the next sync.
fn auto_sync(app: &mut App, config: &Config) {
    if let Some(cfg) = &config.sync
        && let Err(e) = sync::sync(app, cfg)
    {
        eprintln!("td: saved locally, not synced ({e})");
    }
}

fn parse_due(
    due: Option<&str>,
    now: &Zoned,
) -> Result<Option<Timestamp>, CoreError> {
    Ok(due.map(|s| due::parse(s, now)).transpose()?)
}

fn describe(due_secs: i64, now: &Zoned) -> String {
    match Timestamp::from_second(due_secs) {
        Ok(ts) => due::describe(ts, now),
        Err(_) => "?".into(),
    }
}

fn print_table(tasks: &[Task], now: &Zoned) {
    // Only colour when a human is looking.
    let color = std::io::stdout().is_terminal();
    let (red, dim, reset) = if color {
        ("\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let now_secs = now.timestamp().as_second();
    let out = std::io::stdout();
    let mut out = out.lock();
    for t in tasks {
        let mark = if t.done { "x" } else { " " };
        let due = t.due.map(|d| describe(d, now)).unwrap_or_default();
        let style = match t.due {
            _ if t.done => dim,
            Some(d) if d <= now_secs => red,
            _ => "",
        };
        let _ = writeln!(
            out,
            "{style}{:>4}  [{mark}]  {due:<16} {}{reset}",
            t.id, t.title
        );
    }
}
