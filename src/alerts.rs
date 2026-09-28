//! Per-task notification settings: when to remind, and how often to nag.
//!
//! Both are stored (and synced) as a nullable minute count, see
//! `sql/migrate_2.sql`; these types are the only code that knows the
//! encoding.

use serde::Serialize;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("can't understand {0:?} (try: default, off, 15m, 2h, 1d)")]
pub struct AlertError(String);

/// When to send the first notification for a due task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Remind {
    /// Use the server's `lead_minutes`.
    #[default]
    Default,
    Off,
    /// Minutes before the due time; 0 is "at the due time".
    Before(u32),
}

/// How often to re-notify while a task is overdue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Nag {
    /// Use the server's `nag_minutes`.
    #[default]
    Default,
    Off,
    /// Minutes between nags; never 0.
    Every(u32),
}

impl Remind {
    /// Choices offered by the TUI form, in cycling order.
    pub const PRESETS: &[Remind] = &[
        Remind::Default,
        Remind::Before(0),
        Remind::Before(5),
        Remind::Before(15),
        Remind::Before(30),
        Remind::Before(60),
        Remind::Before(24 * 60),
        Remind::Off,
    ];

    pub fn to_raw(self) -> Option<i64> {
        match self {
            Remind::Default => None,
            Remind::Off => Some(-1),
            Remind::Before(m) => Some(m.into()),
        }
    }

    pub fn from_raw(raw: Option<i64>) -> Self {
        match raw {
            None => Remind::Default,
            Some(n) if n < 0 => Remind::Off,
            Some(n) => Remind::Before(clamp(n)),
        }
    }

    /// Accepts `default`, `off`, `due` (at the due time) or a duration.
    pub fn parse(s: &str) -> Result<Self, AlertError> {
        match s.trim().to_lowercase().as_str() {
            "default" => Ok(Remind::Default),
            "off" | "none" | "never" => Ok(Remind::Off),
            "due" | "at" | "0" => Ok(Remind::Before(0)),
            d => parse_minutes(d)
                .map(Remind::Before)
                .ok_or_else(|| AlertError(s.to_string())),
        }
    }

    /// Minutes of lead time, given the server default; `None` if off.
    pub fn resolve(self, default: u32) -> Option<u32> {
        match self {
            Remind::Default => Some(default),
            Remind::Off => None,
            Remind::Before(m) => Some(m),
        }
    }

    pub fn label(self) -> String {
        match self {
            Remind::Default => "default".into(),
            Remind::Off => "off".into(),
            Remind::Before(0) => "at due time".into(),
            Remind::Before(m) => format!("{} before", minutes(m)),
        }
    }
}

impl Nag {
    pub const PRESETS: &[Nag] = &[
        Nag::Default,
        Nag::Every(15),
        Nag::Every(30),
        Nag::Every(60),
        Nag::Every(2 * 60),
        Nag::Every(4 * 60),
        Nag::Every(24 * 60),
        Nag::Off,
    ];

    pub fn to_raw(self) -> Option<i64> {
        match self {
            Nag::Default => None,
            Nag::Off => Some(0),
            Nag::Every(m) => Some(m.into()),
        }
    }

    pub fn from_raw(raw: Option<i64>) -> Self {
        match raw {
            None => Nag::Default,
            Some(n) if n <= 0 => Nag::Off,
            Some(n) => Nag::Every(clamp(n)),
        }
    }

    /// Accepts `default`, `off` or a duration.
    pub fn parse(s: &str) -> Result<Self, AlertError> {
        match s.trim().to_lowercase().as_str() {
            "default" => Ok(Nag::Default),
            "off" | "none" | "never" | "0" => Ok(Nag::Off),
            d => match parse_minutes(d) {
                Some(0) => Ok(Nag::Off),
                Some(m) => Ok(Nag::Every(m)),
                None => Err(AlertError(s.to_string())),
            },
        }
    }

    /// Minutes between nags, given the server default; `None` if off.
    pub fn resolve(self, default: u32) -> Option<u32> {
        match self {
            Nag::Default => (default > 0).then_some(default),
            Nag::Off => None,
            Nag::Every(m) => Some(m),
        }
    }

    pub fn label(self) -> String {
        match self {
            Nag::Default => "default".into(),
            Nag::Off => "off".into(),
            Nag::Every(m) => format!("every {}", minutes(m)),
        }
    }
}

/// Steps through `presets` from `current`. A value not in the list (e.g.
/// set from the CLI) steps to the first or last preset.
pub fn cycle<T: Copy + PartialEq>(
    presets: &[T],
    current: T,
    forward: bool,
) -> T {
    let n = presets.len();
    let i = match presets.iter().position(|&p| p == current) {
        Some(i) if forward => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None if forward => 0,
        None => n - 1,
    };
    presets[i]
}

fn clamp(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// "15m", "2h", "1d", "1w", or a combination like "1h30m".
fn parse_minutes(s: &str) -> Option<u32> {
    let mut total: u32 = 0;
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let (n, tail) = rest.split_at(digits);
        let n: u32 = n.parse().ok()?;
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let scale = match unit.trim() {
            "m" | "min" | "mins" => 1,
            "h" | "hr" | "hrs" => 60,
            "d" | "day" | "days" => 24 * 60,
            "w" | "wk" | "wks" => 7 * 24 * 60,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(scale)?)?;
        rest = tail.trim_start();
    }
    Some(total)
}

/// 90 -> "1h 30m", 1440 -> "1d".
fn minutes(m: u32) -> String {
    let (d, h, m) = (m / 1440, m / 60 % 24, m % 60);
    let parts: Vec<String> = [(d, "d"), (h, "h"), (m, "m")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    if parts.is_empty() {
        "0m".into()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_round_trips() {
        for &r in Remind::PRESETS {
            assert_eq!(Remind::from_raw(r.to_raw()), r);
        }
        for &n in Nag::PRESETS {
            assert_eq!(Nag::from_raw(n.to_raw()), n);
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(Remind::parse("15m"), Ok(Remind::Before(15)));
        assert_eq!(Remind::parse("1h30m"), Ok(Remind::Before(90)));
        assert_eq!(Remind::parse("due"), Ok(Remind::Before(0)));
        assert_eq!(Remind::parse("OFF"), Ok(Remind::Off));
        assert_eq!(Nag::parse("2h"), Ok(Nag::Every(120)));
        assert_eq!(Nag::parse("0m"), Ok(Nag::Off));
        assert_eq!(Nag::parse("default"), Ok(Nag::Default));
        for bad in ["", "soon", "15", "5x", "m"] {
            assert!(Remind::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(Remind::Before(0).label(), "at due time");
        assert_eq!(Remind::Before(90).label(), "1h 30m before");
        assert_eq!(Nag::Every(1440).label(), "every 1d");
    }

    #[test]
    fn cycling_wraps_and_handles_custom_values() {
        let p = Remind::PRESETS;
        assert_eq!(cycle(p, Remind::Default, true), Remind::Before(0));
        assert_eq!(cycle(p, Remind::Default, false), Remind::Off);
        assert_eq!(cycle(p, Remind::Off, true), Remind::Default);
        assert_eq!(cycle(p, Remind::Before(45), true), Remind::Default);
    }

    #[test]
    fn resolve_uses_defaults() {
        assert_eq!(Remind::Default.resolve(10), Some(10));
        assert_eq!(Remind::Off.resolve(10), None);
        assert_eq!(Nag::Default.resolve(0), None);
        assert_eq!(Nag::Every(5).resolve(120), Some(5));
    }
}
