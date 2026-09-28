//! Pushes due-date reminders and overdue nags to a phone through ntfy.
//!
//! Runs inside `td serve` on its own thread and connection. Each tick it
//! asks the database what needs sending (`plan`), sends it, and records
//! each successful send so it isn't repeated; a failed send is simply
//! retried on the next tick.

use std::time::Duration;

use jiff::{Timestamp, Zoned, tz::TimeZone};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::alerts::{Nag, Remind};
use crate::config::NtfyConfig;
use crate::due;

const DUE_TASKS: &str = include_str!("sql/server/due_tasks.sql");
const RECORD_NOTIFIED: &str = include_str!("sql/server/record_notified.sql");

const TICK: Duration = Duration::from_secs(30);

/// A reminder first seen this long after its due time is sent as
/// "overdue" instead of "due now" (e.g. after the server was down).
const LATE_GRACE_SECS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// First notification for this due time.
    Reminder,
    /// Still not done, and `nag_minutes` passed since the last one.
    Nag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub uuid: String,
    pub title: String,
    pub due: i64,
    pub kind: Kind,
}

/// What should be sent at `now` (unix seconds).
pub fn plan(
    conn: &Connection,
    now: i64,
    cfg: &NtfyConfig,
) -> rusqlite::Result<Vec<Notification>> {
    let mut stmt = conn.prepare_cached(DUE_TASKS)?;
    let rows = stmt.query_map(params![now, cfg.lead_minutes], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            Remind::from_raw(r.get(3)?),
            Nag::from_raw(r.get(4)?),
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (uuid, title, due, remind, nag, sent_for, last) = row?;
        // Whatever was sent for an earlier due time no longer counts.
        let handled = sent_for == Some(due);
        let nag = nag.resolve(cfg.nag_minutes).map(|m| i64::from(m) * 60);
        // Nags are timed from the last notification; with reminders off,
        // from the due time, so the first nag comes one interval late.
        let since = if handled { last.unwrap_or(due) } else { due };
        let kind = if !handled && remind.resolve(cfg.lead_minutes).is_some() {
            // New task, or its due date moved: remind about the new time.
            Kind::Reminder
        } else if let Some(nag) = nag
            && now >= due
            && now - since >= nag
        {
            Kind::Nag
        } else {
            continue;
        };
        out.push(Notification {
            uuid,
            title,
            due,
            kind,
        });
    }
    Ok(out)
}

pub fn record(
    conn: &Connection,
    n: &Notification,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(RECORD_NOTIFIED, params![n.uuid, n.due, now])?;
    Ok(())
}

/// The ntfy JSON publish body; see
/// https://docs.ntfy.sh/publish/#publish-as-json
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Message {
    topic: String,
    title: String,
    message: String,
    priority: u8,
    tags: Vec<&'static str>,
}

pub fn message(
    n: &Notification,
    now: i64,
    tz: &TimeZone,
    topic: &str,
) -> Message {
    let now_z = Timestamp::from_second(now)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(tz.clone());
    let when = Timestamp::from_second(n.due)
        .map(|d| due::describe(d, &now_z))
        .unwrap_or_default();
    let late = now - n.due;
    let (body, priority, tag) = if late > LATE_GRACE_SECS {
        let body = format!("Overdue by {} (due {when})", humanize(late));
        let priority = if n.kind == Kind::Nag { 3 } else { 4 };
        (body, priority, "warning")
    } else if late >= 0 {
        (format!("Due now ({when})"), 4, "alarm_clock")
    } else {
        let body = format!("Due in {} ({when})", humanize(-late));
        (body, 4, "alarm_clock")
    };
    Message {
        topic: topic.to_string(),
        title: n.title.clone(),
        message: body,
        priority,
        tags: vec![tag],
    }
}

/// "45s", "5m", "2h", "2h 5m", "3d", "3d 4h".
fn humanize(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// One pass: plan, send, record. Returns how many were sent.
pub fn tick(
    conn: &Connection,
    now: i64,
    cfg: &NtfyConfig,
    tz: &TimeZone,
    send: &mut dyn FnMut(&Message) -> Result<(), String>,
) -> rusqlite::Result<usize> {
    let mut sent = 0;
    for n in plan(conn, now, cfg)? {
        let msg = message(&n, now, tz, &cfg.topic);
        match send(&msg) {
            Ok(()) => {
                record(conn, &n, now)?;
                sent += 1;
            }
            Err(e) => eprintln!("ntfy: sending {:?} failed: {e}", n.title),
        }
    }
    Ok(sent)
}

/// Starts the notifier thread. Runs until the process exits.
/// Posts messages to the configured ntfy server.
pub struct Sender {
    agent: ureq::Agent,
    url: String,
    token: Option<String>,
}

impl Sender {
    pub fn new(cfg: &NtfyConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Sender {
            agent,
            url: cfg.url.trim_end_matches('/').to_string(),
            token: cfg.token.clone(),
        }
    }

    pub fn send(&self, msg: &Message) -> Result<(), String> {
        let mut req = self.agent.post(&self.url);
        if let Some(token) = &self.token {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        req.send_json(msg).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// A one-off message to check the phone is subscribed.
pub fn test_message(topic: &str) -> Message {
    Message {
        topic: topic.to_string(),
        title: "todont is set up".into(),
        message: "Reminders from td serve will show up here.".into(),
        priority: 3,
        tags: vec!["tada"],
    }
}

pub fn spawn(conn: Connection, cfg: NtfyConfig, tz: TimeZone) {
    let sender = Sender::new(&cfg);
    std::thread::spawn(move || {
        let mut send = |msg: &Message| sender.send(msg);
        loop {
            let now = Zoned::now().timestamp().as_second();
            match tick(&conn, now, &cfg, &tz, &mut send) {
                Ok(0) => {}
                Ok(n) => eprintln!("ntfy: sent {n}"),
                Err(e) => eprintln!("ntfy: {e}"),
            }
            std::thread::sleep(TICK);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Change, SyncRequest};
    use crate::server;

    const HOUR: i64 = 3_600;
    const T0: i64 = 1_790_000_000;

    fn cfg(lead_minutes: u32, nag_minutes: u32) -> NtfyConfig {
        NtfyConfig {
            topic: "t".into(),
            url: "http://unused".into(),
            token: None,
            lead_minutes,
            nag_minutes,
        }
    }

    fn put(conn: &mut Connection, uuid: &str, due: i64, done: bool, v: i64) {
        put_with(conn, uuid, due, done, v, Remind::Default, Nag::Default);
    }

    fn put_with(
        conn: &mut Connection,
        uuid: &str,
        due: i64,
        done: bool,
        v: i64,
        remind: Remind,
        nag: Nag,
    ) {
        let c = Change {
            uuid: uuid.into(),
            title: uuid.into(),
            done,
            due: Some(due),
            created: 0,
            updated: v,
            deleted: false,
            remind: remind.to_raw(),
            nag: nag.to_raw(),
        };
        server::apply(
            conn,
            &SyncRequest {
                cursor: 0,
                changes: vec![c],
            },
        )
        .unwrap();
    }

    /// Runs a tick at `now`, returning the titles sent.
    fn run(conn: &Connection, now: i64, cfg: &NtfyConfig) -> Vec<String> {
        let mut sent = Vec::new();
        tick(conn, now, cfg, &TimeZone::UTC, &mut |m| {
            sent.push(format!("{}: {}", m.title, m.message));
            Ok(())
        })
        .unwrap();
        sent
    }

    #[test]
    fn reminds_once_at_due_time_then_nags() {
        let mut conn = server::open_db_in_memory().unwrap();
        let cfg = cfg(0, 120);
        put(&mut conn, "a", T0, false, 1);

        assert!(run(&conn, T0 - 60, &cfg).is_empty());
        assert_eq!(run(&conn, T0, &cfg).len(), 1);
        assert!(run(&conn, T0 + 30, &cfg).is_empty());
        assert!(run(&conn, T0 + HOUR, &cfg).is_empty());
        let nag = run(&conn, T0 + 2 * HOUR, &cfg);
        assert_eq!(nag.len(), 1);
        assert!(nag[0].contains("Overdue by 2h"), "{nag:?}");
        // Next nag is measured from the last one.
        assert!(run(&conn, T0 + 3 * HOUR, &cfg).is_empty());
        assert_eq!(run(&conn, T0 + 4 * HOUR, &cfg).len(), 1);
    }

    #[test]
    fn done_tasks_stop_nagging() {
        let mut conn = server::open_db_in_memory().unwrap();
        let cfg = cfg(0, 60);
        put(&mut conn, "a", T0, false, 1);
        assert_eq!(run(&conn, T0, &cfg).len(), 1);
        put(&mut conn, "a", T0, true, 2);
        assert!(run(&conn, T0 + 5 * HOUR, &cfg).is_empty());
    }

    #[test]
    fn lead_time_and_rescheduling() {
        let mut conn = server::open_db_in_memory().unwrap();
        let cfg = cfg(15, 0);
        put(&mut conn, "a", T0, false, 1);
        let sent = run(&conn, T0 - 15 * 60, &cfg);
        assert!(sent[0].contains("Due in 15m"), "{sent:?}");

        // Moving the due date earns a fresh reminder for the new time.
        put(&mut conn, "a", T0 + HOUR, false, 2);
        assert!(run(&conn, T0, &cfg).is_empty());
        assert_eq!(run(&conn, T0 + 45 * 60, &cfg).len(), 1);
        // Nags are off.
        assert!(run(&conn, T0 + 10 * HOUR, &cfg).is_empty());
    }

    #[test]
    fn failed_sends_are_retried() {
        let mut conn = server::open_db_in_memory().unwrap();
        let cfg = cfg(0, 0);
        put(&mut conn, "a", T0, false, 1);
        let n = tick(&conn, T0, &cfg, &TimeZone::UTC, &mut |_| {
            Err("offline".into())
        })
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(run(&conn, T0 + 30, &cfg).len(), 1);
    }

    #[test]
    fn stale_reminders_say_overdue() {
        let mut conn = server::open_db_in_memory().unwrap();
        put(&mut conn, "a", T0 - 26 * HOUR, false, 1);
        let sent = run(&conn, T0, &cfg(0, 0));
        assert!(sent[0].starts_with("a: Overdue by 1d 2h"), "{sent:?}");
    }

    #[test]
    fn per_task_settings_override_defaults() {
        let mut conn = server::open_db_in_memory().unwrap();
        // Defaults: remind at due time, nag every 2h.
        let cfg = cfg(0, 120);
        let (early, quiet) = (Remind::Before(60), Nag::Off);
        put_with(&mut conn, "early", T0, false, 1, early, quiet);
        put_with(
            &mut conn,
            "nag only",
            T0,
            false,
            1,
            Remind::Off,
            Nag::Every(30),
        );

        let sent = run(&conn, T0 - HOUR, &cfg);
        assert!(sent.len() == 1 && sent[0].starts_with("early: Due in 1h"));
        // "nag only" gets nothing at its due time...
        assert!(run(&conn, T0, &cfg).is_empty());
        // ...then its first nag one interval later.
        let sent = run(&conn, T0 + 30 * 60, &cfg);
        assert!(
            sent.len() == 1 && sent[0].starts_with("nag only: Overdue by 30m")
        );
        // "early" never nags, even long after.
        assert!(
            run(&conn, T0 + 10 * HOUR, &cfg)
                .iter()
                .all(|s| s.starts_with("nag only"))
        );
    }

    #[test]
    fn humanize_units() {
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(300), "5m");
        assert_eq!(humanize(2 * HOUR + 300), "2h 5m");
        assert_eq!(humanize(3 * 86_400 + 4 * HOUR), "3d 4h");
    }
}
