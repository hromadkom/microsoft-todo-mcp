//! Timezone resolution (m5). A To Do due date is an all-day date Graph stores
//! as an instant, so the naive reading is wrong by a day for much of the planet
//! for part of every day. The rule: **the civil date of `dateTime` interpreted in
//! the zone named by `timeZone`, converted into the effective zone, time of day
//! discarded.**
//!
//! This is the ONLY caller of `from_local_datetime` in the crate (gate 10).
//! `with_ymd_and_hms(..).unwrap()` panics on the three dates in §5 where local
//! midnight does not exist.

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use windows_timezones::WindowsTimezone;

use crate::graph::models::DateTimeTimeZone;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TzError {
    UnknownZone(String),
    BadDateTime(String),
    /// No valid local time within 3 h of the requested wall clock.
    NoValidLocalTime,
}

pub struct Resolved {
    pub instant: DateTime<Utc>,
    /// The field every predicate reads.
    pub local: DateTime<Tz>,
}

/// IANA first (covers "UTC"), else the Windows name via `tzdb_id()`, else None.
/// The `None` arm never falls back to UTC — a guessed zone is the §1 bug again.
pub fn parse_graph_tz(s: &str) -> Option<Tz> {
    let s = s.trim();
    if let Ok(tz) = s.parse::<Tz>() {
        return Some(tz);
    }
    if let Ok(w) = s.parse::<WindowsTimezone>() {
        return w.tzdb_id().parse::<Tz>().ok();
    }
    None
}

/// Parse Graph's `dateTime` — `%Y-%m-%dT%H:%M:%S%.f`, then without the fraction,
/// then a bare date.
pub fn parse_naive(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim().trim_end_matches('Z');
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .map(|d| d.and_time(NaiveTime::MIN))
        })
}

/// Map a wall clock in `src` to an instant. Handles all three `LocalResult`
/// arms with no `_ =>`: ambiguous takes the EARLIER instant (same local date
/// either way); a gap shifts forward in 15-minute steps.
pub fn local_to_instant(src: Tz, naive: NaiveDateTime) -> Result<DateTime<Utc>, TzError> {
    match src.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(a, _) => Ok(a.with_timezone(&Utc)),
        chrono::LocalResult::None => shift_forward_to_valid(src, naive),
    }
}

/// 15-minute steps, up to 3 h, until a valid local time exists. The local DATE
/// is unchanged on every measured transition (m5 §5).
fn shift_forward_to_valid(src: Tz, naive: NaiveDateTime) -> Result<DateTime<Utc>, TzError> {
    let mut probe = naive;
    for _ in 0..12 {
        probe += chrono::Duration::minutes(15);
        match src.from_local_datetime(&probe) {
            chrono::LocalResult::Single(dt) => return Ok(dt.with_timezone(&Utc)),
            chrono::LocalResult::Ambiguous(a, _) => return Ok(a.with_timezone(&Utc)),
            chrono::LocalResult::None => {}
        }
    }
    Err(TzError::NoValidLocalTime)
}

/// Resolve a Graph `dateTimeTimeZone` into the effective zone `tz`.
pub fn resolve(dtz: &DateTimeTimeZone, tz: Tz) -> Result<Resolved, TzError> {
    let src = parse_graph_tz(&dtz.time_zone)
        .ok_or_else(|| TzError::UnknownZone(dtz.time_zone.clone()))?;
    let naive =
        parse_naive(&dtz.date_time).ok_or_else(|| TzError::BadDateTime(dtz.date_time.clone()))?;
    let instant = local_to_instant(src, naive)?;
    Ok(Resolved {
        instant,
        local: instant.with_timezone(&tz),
    })
}

/// The local civil date of a Graph date, or `Err` with the raw zone string.
pub fn local_date(dtz: &DateTimeTimeZone, tz: Tz) -> Result<NaiveDate, TzError> {
    resolve(dtz, tz).map(|r| r.local.date_naive())
}

/// Parse an RFC 3339 instant such as `createdDateTime`. Graph emits seven
/// fractional digits; chrono accepts any count.
pub fn parse_instant(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
        .or_else(|| parse_naive(s).map(|n| Utc.from_utc_datetime(&n)))
}

/// `today` in the effective zone from the injected clock.
pub fn today_in(now: DateTime<Utc>, tz: Tz) -> NaiveDate {
    now.with_timezone(&tz).date_naive()
}

/// Calendar arithmetic, never `+ 86400 * h`.
pub fn add_days(d: NaiveDate, n: u64) -> NaiveDate {
    d.checked_add_days(Days::new(n)).unwrap_or(d)
}

pub fn sub_days(d: NaiveDate, n: u64) -> NaiveDate {
    d.checked_sub_days(Days::new(n)).unwrap_or(d)
}

// ---------------------------------------------------------------------------
// Outbound: the Windows name for `Prefer` and for date writes (m5 §8–§9)

/// Hand-written, outbound only, UNVERIFIED against Graph until probe J runs.
/// Never fed back through `parse_graph_tz`.
const ALIAS_FALLBACK: &[(&str, &str)] = &[
    // CLDR's primary for the Windows "UTC" zone is Etc/UTC, so the bare "UTC"
    // Tz — the configured default — needs its own row or every dated write
    // under the default configuration would refuse.
    ("UTC", "UTC"),
    ("Etc/UTC", "UTC"),
    ("Etc/GMT", "UTC"),
    ("Asia/Kolkata", "India Standard Time"),
    ("Europe/Kyiv", "FLE Standard Time"),
    ("Asia/Ho_Chi_Minh", "SE Asia Standard Time"),
    ("America/Nuuk", "Greenland Standard Time"),
    ("Europe/Uzhgorod", "FLE Standard Time"),
    ("Europe/Zaporozhye", "FLE Standard Time"),
];

/// The Windows name for `tz`, else an alias fallback, else `None`.
pub fn prefer_tz_value(tz: Tz) -> Option<&'static str> {
    if let Ok(w) = WindowsTimezone::try_from(tz) {
        return Some(w.name());
    }
    ALIAS_FALLBACK
        .iter()
        .find(|(iana, _)| *iana == tz.name())
        .map(|(_, w)| *w)
}

/// CLDR's older spelling of the same zone, when one exists and maps.
fn older_spelling(tz: Tz) -> Option<Tz> {
    let candidates: &[(&str, &str)] = &[
        ("Europe/Kyiv", "Europe/Kiev"),
        ("Asia/Kolkata", "Asia/Calcutta"),
        ("Asia/Ho_Chi_Minh", "Asia/Saigon"),
        ("America/Nuuk", "America/Godthab"),
        ("Asia/Yangon", "Asia/Rangoon"),
        ("Europe/Uzhgorod", "Europe/Kiev"),
        ("Europe/Zaporozhye", "Europe/Kiev"),
    ];
    candidates
        .iter()
        .find(|(new, _)| *new == tz.name())
        .and_then(|(_, old)| old.parse::<Tz>().ok())
        .filter(|old| WindowsTimezone::try_from(*old).is_ok())
}

/// A zone that really maps to a Windows name, as close as we can get: the
/// older CLDR spelling, else the first `TZ_VARIANTS` entry with a Windows name
/// and the same UTC offset right now, else UTC.
pub fn suggest_writable_zone(tz: Tz, now: DateTime<Utc>) -> Tz {
    if let Some(old) = older_spelling(tz) {
        return old;
    }
    let want = now.with_timezone(&tz).offset().fix();
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .find(|cand| {
            WindowsTimezone::try_from(*cand).is_ok()
                && now.with_timezone(cand).offset().fix() == want
        })
        .unwrap_or(Tz::UTC)
}

/// What a date write puts in `timeZone`, or the §8 refusal text.
pub fn outbound_tz_name(tz: Tz, now: DateTime<Utc>) -> Result<String, String> {
    if let Some(w) = prefer_tz_value(tz) {
        return Ok(w.to_string());
    }
    let suggestion = suggest_writable_zone(tz, now);
    let extra = if suggestion == Tz::UTC {
        " Dates would then be UTC-anchored."
    } else {
        ""
    };
    Err(format!(
        "Cannot write a date in time zone \"{}\": Microsoft Graph accepts only Windows time-zone names \
         or a specific list of IANA names, and this zone is in neither. Set TODO_MCP_TZ (or the timezone \
         argument) to a nearby supported zone such as \"{}\".{extra}",
        tz.name(),
        suggestion.name()
    ))
}

/// `2026-08-25` → `2026-08-25T00:00:00.0000000` (Graph's seven-digit form).
pub fn graph_midnight(date: NaiveDate) -> String {
    format!("{}T00:00:00.0000000", date.format("%Y-%m-%d"))
}

/// `2026-08-25T09:00` → `2026-08-25T09:00:00.0000000`.
pub fn graph_wall_clock(dt: NaiveDateTime) -> String {
    format!("{}.0000000", dt.format("%Y-%m-%dT%H:%M:%S"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dtz(dt: &str, tz: &str) -> DateTimeTimeZone {
        DateTimeTimeZone {
            date_time: dt.into(),
            time_zone: tz.into(),
        }
    }

    #[test]
    fn parse_graph_tz_ladder() {
        assert_eq!(parse_graph_tz("UTC"), Some(Tz::UTC));
        assert_eq!(parse_graph_tz("Etc/UTC"), Some(Tz::Etc__UTC));
        assert_eq!(parse_graph_tz("Europe/Berlin"), Some(Tz::Europe__Berlin));
        assert_eq!(
            parse_graph_tz("W. Europe Standard Time"),
            Some(Tz::Europe__Berlin)
        );
        assert_eq!(
            parse_graph_tz("Central Europe Standard Time"),
            Some(Tz::Europe__Budapest)
        );
        assert_eq!(
            parse_graph_tz("Pacific Standard Time"),
            Some(Tz::America__Los_Angeles)
        );
        assert_eq!(
            parse_graph_tz("Eastern Standard Time"),
            Some(Tz::America__New_York)
        );
        assert_eq!(
            parse_graph_tz("GMT Standard Time"),
            Some(Tz::Europe__London)
        );
        assert_eq!(parse_graph_tz("tzone://Microsoft/Utc"), None);
        assert_eq!(parse_graph_tz("Customized Time Zone"), None);
    }

    #[test]
    fn the_off_by_one_is_eliminated() {
        // UI due date 2023-04-15 at UTC+2, stored as 2023-04-14T22:00Z.
        let d = dtz("2023-04-14T22:00:00.0000000", "UTC");
        assert_eq!(
            local_date(&d, Tz::Europe__Prague).unwrap(),
            NaiveDate::from_ymd_opt(2023, 4, 15).unwrap()
        );
        assert_eq!(
            local_date(&d, Tz::UTC).unwrap(),
            NaiveDate::from_ymd_opt(2023, 4, 14).unwrap()
        );
        // Microsoft's own sample: 04:00Z is Aug 25 in a western mailbox.
        let d = dtz("2020-08-25T04:00:00.0000000", "UTC");
        assert_eq!(
            local_date(&d, Tz::America__New_York).unwrap(),
            NaiveDate::from_ymd_opt(2020, 8, 25).unwrap()
        );
    }

    #[test]
    fn nonexistent_local_midnight_shifts_forward_and_keeps_the_date() {
        for (zone, date) in [
            (Tz::America__Santiago, "2026-09-06"),
            (Tz::Asia__Beirut, "2026-03-29"),
            (Tz::America__Havana, "2026-03-08"),
        ] {
            let d = dtz(&format!("{date}T00:00:00.0000000"), zone.name());
            let r = resolve(&d, zone).unwrap();
            assert_eq!(r.local.date_naive().to_string(), date, "{zone}");
            assert_eq!(r.local.format("%H:%M").to_string(), "01:00", "{zone}");
        }
    }

    #[test]
    fn ambiguous_fall_back_hour_takes_the_earlier_instant() {
        let d = dtz("2026-10-25T02:30:00.0000000", "Europe/Prague");
        let r = resolve(&d, Tz::Europe__Prague).unwrap();
        assert_eq!(r.local.date_naive().to_string(), "2026-10-25");
        assert_eq!(r.local.format("%z").to_string(), "+0200");
    }

    #[test]
    fn half_hour_offset_transition_resolves() {
        let d = dtz("2026-10-04T02:15:00.0000000", "Australia/Lord_Howe");
        assert!(resolve(&d, Tz::Australia__Lord_Howe).is_ok());
    }

    #[test]
    fn year_boundary_is_plain_calendar_arithmetic() {
        let d = dtz("2026-12-31T23:00:00.0000000", "UTC");
        assert_eq!(
            local_date(&d, Tz::Europe__Prague).unwrap().to_string(),
            "2027-01-01"
        );
        assert_eq!(local_date(&d, Tz::UTC).unwrap().to_string(), "2026-12-31");
    }

    #[test]
    fn unknown_zone_is_an_error_never_utc() {
        let d = dtz("2026-01-01T00:00:00", "tzone://Microsoft/Custom");
        assert!(matches!(resolve(&d, Tz::UTC), Err(TzError::UnknownZone(_))));
    }

    #[test]
    fn round_trip_is_lossy_and_pinned() {
        assert_eq!(
            prefer_tz_value(Tz::Europe__Prague),
            Some("Central Europe Standard Time")
        );
        assert_eq!(
            parse_graph_tz("Central Europe Standard Time"),
            Some(Tz::Europe__Budapest)
        );
    }

    #[test]
    fn alias_fallback_and_suggestions() {
        assert_eq!(prefer_tz_value(Tz::Europe__Kyiv), Some("FLE Standard Time"));
        assert_eq!(
            prefer_tz_value(Tz::Asia__Kolkata),
            Some("India Standard Time")
        );
        assert_eq!(prefer_tz_value(Tz::UTC), Some("UTC"));
        let now = Utc::now_pinned();
        assert_eq!(
            suggest_writable_zone(Tz::Europe__Kyiv, now),
            Tz::Europe__Kiev
        );
        assert!(WindowsTimezone::try_from(Tz::Europe__Kiev).is_ok());
        let s = suggest_writable_zone(Tz::Antarctica__Troll, now);
        assert!(WindowsTimezone::try_from(s).is_ok() || s == Tz::UTC);
        assert!(outbound_tz_name(Tz::Europe__Prague, now).is_ok());
    }

    #[test]
    fn instant_parsing_accepts_graphs_seven_digits() {
        assert!(parse_instant("2020-08-18T09:03:05.8339192Z").is_some());
        assert!(parse_instant("2020-08-18T09:03:05Z").is_some());
        assert!(parse_instant("nope").is_none());
    }

    trait Pinned {
        fn now_pinned() -> DateTime<Utc>;
    }
    impl Pinned for Utc {
        fn now_pinned() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap()
        }
    }
}
