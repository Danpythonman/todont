//! Parsing and describing due dates in the user's local time zone. Shared by
//! both frontends; never touches the database.
//!
//! Accepted forms (case-insensitive; commas and the words on/the/of/at/by
//! are ignored):
//!   now | +30m | +2h | +3d | +1w | in 2h
//!   today | tomorrow | tmr | mon..sun | next fri          (at 09:00)
//!   2026-10-01 | 2026/10/01
//!   sep 30 | september 30th | 30 sept | sep 30 2027      (month names)
//!   the 30th                              (the next 30th of a month)
//!   17:00 | 5pm | 5:30pm | noon           (today, or tomorrow if passed)
//!   <date> <time> | <time> <date>         e.g. "fri 5pm", "5pm sep 30"
//!
//! A date without a year is the next time it comes around.

use jiff::{
    Span, Timestamp, Zoned,
    civil::{Date, Time, Weekday},
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "can't understand due date {0:?} \
     (try: tomorrow 17:00, fri 5pm, sep 30, 2026-10-01, +2h)"
)]
pub struct DueError(String);

/// Time of day used when only a date is given.
const DEFAULT_TIME: Time = Time::constant(9, 0, 0, 0);

pub fn parse(input: &str, now: &Zoned) -> Result<Timestamp, DueError> {
    let err = || DueError(input.to_string());
    // Glue a detached meridiem onto its number: "5 pm" -> "5pm".
    let s = input
        .trim()
        .to_lowercase()
        .replace(" am", "am")
        .replace(" pm", "pm");

    if s == "now" {
        return Ok(now.timestamp());
    }
    if let Some(rest) = s.strip_prefix('+').or_else(|| s.strip_prefix("in ")) {
        return relative(rest.trim(), now).ok_or_else(err);
    }

    let mut words: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty() && !FILLER.contains(w))
        .collect();
    // The time may come last ("fri 5pm") or first ("5pm fri").
    let time = if let Some(t) = words.last().and_then(|w| parse_time(w)) {
        words.pop();
        Some(t)
    } else if let Some(t) = words.first().and_then(|w| parse_time(w)) {
        words.remove(0);
        Some(t)
    } else {
        None
    };
    let today = now.date();
    let (date, time) = match (words.is_empty(), time) {
        (true, None) => return Err(err()),
        // A bare time: its next occurrence.
        (true, Some(t)) if today.to_datetime(t) > now.datetime() => (today, t),
        (true, Some(t)) => (today.tomorrow().map_err(|_| err())?, t),
        (false, t) => (
            parse_date(&words, today).ok_or_else(err)?,
            t.unwrap_or(DEFAULT_TIME),
        ),
    };
    date.to_datetime(time)
        .to_zoned(now.time_zone().clone())
        .map(|z| z.timestamp())
        .map_err(|_| err())
}

fn relative(s: &str, now: &Zoned) -> Option<Timestamp> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: i64 = n.parse().ok()?;
    let span = match unit.trim() {
        "m" | "min" | "mins" => Span::new().try_minutes(n),
        "h" | "hr" | "hrs" => Span::new().try_hours(n),
        "d" | "day" | "days" => Span::new().try_days(n),
        "w" | "wk" | "wks" => Span::new().try_weeks(n),
        _ => return None,
    }
    .ok()?;
    // Add on the zoned value so "+1d" means the same wall-clock time
    // tomorrow, even across a DST change.
    now.checked_add(span).ok().map(|z| z.timestamp())
}

/// Words that only make a phrase read naturally: "on the 30th of sep".
const FILLER: &[&str] = &["on", "the", "of", "at", "by"];

fn parse_date(words: &[&str], today: Date) -> Option<Date> {
    match words {
        [w] => parse_date_word(w, today),
        ["next", w] => parse_weekday(w).and_then(|wd| next_weekday(today, wd)),
        _ => parse_month_phrase(words, today),
    }
}

fn parse_date_word(s: &str, today: Date) -> Option<Date> {
    match s {
        "today" | "tod" => return Some(today),
        "tomorrow" | "tmr" | "tom" => return today.tomorrow().ok(),
        _ => {}
    }
    if let Some(wd) = parse_weekday(s) {
        return next_weekday(today, wd);
    }
    // A lone day of month needs its suffix: "30th", not "30".
    if s.ends_with(|c: char| c.is_ascii_alphabetic())
        && let Some(day) = parse_day(s)
    {
        return next_day_of_month(today, day);
    }
    s.replace('/', "-").parse().ok()
}

/// The next such day, never today: "fri" on a Friday means next week.
fn next_weekday(today: Date, wd: Weekday) -> Option<Date> {
    today.nth_weekday(1, wd).ok()
}

/// "sep 30", "30 sep", "september 30th 2027", "30 sept 2027".
fn parse_month_phrase(words: &[&str], today: Date) -> Option<Date> {
    let i = words.iter().position(|w| parse_month(w).is_some())?;
    let month = parse_month(words[i])?;
    let mut others = words.to_vec();
    others.remove(i);
    // The month comes first or second; the year, if any, last.
    let (day, year) = match others.as_slice() {
        [d] => (parse_day(d)?, None),
        [d, y] if i <= 1 => (parse_day(d)?, Some(parse_year(y)?)),
        _ => return None,
    };
    match year {
        Some(y) => Date::new(y, month, day).ok(),
        None => next_month_day(today, month, day),
    }
}

/// The next `month`/`day` on or after `today`, e.g. "sep 30" in October
/// means next year. Feb 29 may be up to 8 years away.
fn next_month_day(today: Date, month: i8, day: i8) -> Option<Date> {
    (today.year()..=today.year().checked_add(8)?)
        .filter_map(|y| Date::new(y, month, day).ok())
        .find(|d| *d >= today)
}

/// The next date on or after `today` with this day of month; "31st"
/// skips months that don't have one.
fn next_day_of_month(today: Date, day: i8) -> Option<Date> {
    let mut month = today.first_of_month();
    for _ in 0..12 {
        if let Ok(d) = Date::new(month.year(), month.month(), day)
            && d >= today
        {
            return Some(d);
        }
        month = month.checked_add(Span::new().months(1)).ok()?;
    }
    None
}

fn parse_month(s: &str) -> Option<i8> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    if s.len() < 3 {
        return None;
    }
    let i = MONTHS.iter().position(|name| name.starts_with(s))?;
    i8::try_from(i + 1).ok()
}

/// "30", "30th", "1st", "2nd", "3rd".
fn parse_day(s: &str) -> Option<i8> {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suf| s.strip_suffix(suf))
        .unwrap_or(s);
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

fn parse_year(s: &str) -> Option<i16> {
    if s.len() != 4 {
        return None;
    }
    s.parse().ok()
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    const DAYS: [(&str, Weekday); 7] = [
        ("monday", Weekday::Monday),
        ("tuesday", Weekday::Tuesday),
        ("wednesday", Weekday::Wednesday),
        ("thursday", Weekday::Thursday),
        ("friday", Weekday::Friday),
        ("saturday", Weekday::Saturday),
        ("sunday", Weekday::Sunday),
    ];
    if s.len() < 3 {
        return None;
    }
    DAYS.iter()
        .find(|(name, _)| name.starts_with(s))
        .map(|&(_, wd)| wd)
}

fn parse_time(s: &str) -> Option<Time> {
    if s == "noon" {
        return Some(Time::constant(12, 0, 0, 0));
    }
    let (body, meridiem) = if let Some(b) = s.strip_suffix("am") {
        (b, Some(0))
    } else if let Some(b) = s.strip_suffix("pm") {
        (b, Some(12))
    } else {
        (s, None)
    };
    let (h, m): (i8, i8) = match body.split_once(':') {
        Some((h, m)) if m.len() == 2 => (h.parse().ok()?, m.parse().ok()?),
        // A bare number is only a time with am/pm ("5pm"); "17" is not.
        None if meridiem.is_some() => (body.parse().ok()?, 0),
        _ => return None,
    };
    let h = match meridiem {
        Some(offset) if (1..=12).contains(&h) => h % 12 + offset,
        Some(_) => return None,
        None => h,
    };
    Time::new(h, m, 0, 0).ok()
}

/// Short human description relative to `now`, e.g. "today 17:00",
/// "tomorrow 09:00", "Fri 09:00", "Oct 12 09:00", "2027-01-05 09:00".
pub fn describe(due: Timestamp, now: &Zoned) -> String {
    let due = due.to_zoned(now.time_zone().clone());
    let days = (due.date() - now.date()).get_days();
    let day = match days {
        -1 => "yesterday".to_string(),
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        2..=6 => due.strftime("%a").to_string(),
        _ if due.year() == now.year() => due.strftime("%b %d").to_string(),
        _ => due.strftime("%Y-%m-%d").to_string(),
    };
    format!("{day} {}", due.strftime("%H:%M"))
}

#[cfg(test)]
mod tests {
    use jiff::{civil::date, tz::TimeZone};

    use super::*;

    /// Sunday 2026-09-27 10:00 UTC.
    fn now() -> Zoned {
        date(2026, 9, 27)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn at(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> Timestamp {
        date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    fn p(s: &str) -> Timestamp {
        parse(s, &now()).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn relative_offsets() {
        assert_eq!(p("+30m"), at(2026, 9, 27, 10, 30));
        assert_eq!(p("+2h"), at(2026, 9, 27, 12, 0));
        assert_eq!(p("in 3d"), at(2026, 9, 30, 10, 0));
        assert_eq!(p("+1w"), at(2026, 10, 4, 10, 0));
    }

    #[test]
    fn named_days_default_to_nine() {
        assert_eq!(p("today"), at(2026, 9, 27, 9, 0));
        assert_eq!(p("Tomorrow"), at(2026, 9, 28, 9, 0));
        assert_eq!(p("fri"), at(2026, 10, 2, 9, 0));
        // Today is Sunday, so "sunday" is next week.
        assert_eq!(p("sunday"), at(2026, 10, 4, 9, 0));
        assert_eq!(p("2026-12-25"), at(2026, 12, 25, 9, 0));
    }

    #[test]
    fn times_roll_to_tomorrow_once_passed() {
        assert_eq!(p("17:00"), at(2026, 9, 27, 17, 0));
        assert_eq!(p("9am"), at(2026, 9, 28, 9, 0));
        assert_eq!(p("12am"), at(2026, 9, 28, 0, 0));
        assert_eq!(p("12pm"), at(2026, 9, 27, 12, 0));
    }

    #[test]
    fn date_and_time() {
        assert_eq!(p("tomorrow 5:30pm"), at(2026, 9, 28, 17, 30));
        assert_eq!(p("fri 5 pm"), at(2026, 10, 2, 17, 0));
        assert_eq!(p("2026-10-01 8:15"), at(2026, 10, 1, 8, 15));
    }

    #[test]
    fn numeric_dates() {
        assert_eq!(p("2026-09-30"), at(2026, 9, 30, 9, 0));
        assert_eq!(p("2026/09/30"), at(2026, 9, 30, 9, 0));
        assert_eq!(p("2026-09-30 at 17:45"), at(2026, 9, 30, 17, 45));
    }

    #[test]
    fn month_names_in_either_order() {
        let sep30 = at(2026, 9, 30, 9, 0);
        for s in [
            "sep 30",
            "Sept 30",
            "September 30th",
            "30 sep",
            "30th of September",
            "on the 30th of sep",
            "sep 30, 2026",
            "30 september 2026",
        ] {
            assert_eq!(p(s), sep30, "{s:?}");
        }
        assert_eq!(p("Sep 30, 2027 at 5pm"), at(2027, 9, 30, 17, 0));
        assert_eq!(p("5:30pm dec 1st"), at(2026, 12, 1, 17, 30));
    }

    #[test]
    fn dates_without_a_year_are_the_next_one() {
        // Already passed this year.
        assert_eq!(p("sep 1"), at(2027, 9, 1, 9, 0));
        // Today counts, even if its 09:00 has passed.
        assert_eq!(p("sep 27"), at(2026, 9, 27, 9, 0));
        assert_eq!(p("feb 29"), at(2028, 2, 29, 9, 0));
        assert_eq!(p("the 30th"), at(2026, 9, 30, 9, 0));
        assert_eq!(p("1st"), at(2026, 10, 1, 9, 0));
        // September has no 31st.
        assert_eq!(p("the 31st"), at(2026, 10, 31, 9, 0));
    }

    #[test]
    fn other_phrasings() {
        assert_eq!(p("next fri"), at(2026, 10, 2, 9, 0));
        assert_eq!(p("noon"), at(2026, 9, 27, 12, 0));
        assert_eq!(p("tomorrow at noon"), at(2026, 9, 28, 12, 0));
        assert_eq!(p("5pm tomorrow"), at(2026, 9, 28, 17, 0));
        assert_eq!(p("by fri"), at(2026, 10, 2, 9, 0));
    }

    #[test]
    fn rejects_nonsense() {
        for bad in [
            "",
            "soon",
            "17",
            "13pm",
            "+2y",
            "fr",
            "tmr 25:00",
            "a b c",
            "sep",
            "30",
            "sep 31",
            "feb 30 2027",
            "sep 30 27",
            "2026 sep 30",
            "sep 30 oct",
            "the",
            "32nd",
            "9/30",
        ] {
            assert!(parse(bad, &now()).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn describe_is_relative() {
        let n = now();
        assert_eq!(describe(at(2026, 9, 27, 17, 0), &n), "today 17:00");
        assert_eq!(describe(at(2026, 9, 28, 9, 0), &n), "tomorrow 09:00");
        assert_eq!(describe(at(2026, 9, 26, 9, 0), &n), "yesterday 09:00");
        assert_eq!(describe(at(2026, 10, 2, 9, 0), &n), "Fri 09:00");
        assert_eq!(describe(at(2026, 11, 3, 9, 0), &n), "Nov 03 09:00");
        assert_eq!(describe(at(2027, 1, 5, 9, 0), &n), "2027-01-05 09:00");
    }
}
