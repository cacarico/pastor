//! When a job is due. `every` is a plain interval; `cron` is 5-field cron in the
//! head's local time, matched by a small walker rather than a crate: the spec's
//! subset (numbers, `*`, ranges, lists, steps) fits in a page and stays
//! readable next to its tests.

use std::time::Duration;

use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};

use crate::config::parse_duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Every(Duration),
    Cron(CronExpr),
}

impl Schedule {
    /// From a job file's `every` / `cron` fields: exactly one must be set.
    pub fn from_fields(every: Option<&str>, cron: Option<&str>) -> Result<Schedule, String> {
        match (every, cron) {
            (Some(e), None) => {
                let d = parse_duration(e).map_err(|err| format!("every: {err}"))?;
                if d.is_zero() {
                    return Err("every: must not be zero".into());
                }
                Ok(Schedule::Every(d))
            }
            (None, Some(c)) => Ok(Schedule::Cron(CronExpr::parse(c)?)),
            (Some(_), Some(_)) => Err("set either every or cron, not both".into()),
            (None, None) => Err("set every or cron".into()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Every(d) => format!("every {}", describe_duration(*d)),
            Schedule::Cron(c) => format!("cron {}", c.source),
        }
    }

    /// The first instant strictly after `last` at which a run is due. Cron is
    /// evaluated in the head's local time zone and converted back.
    pub fn next_after(&self, last: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Every(d) => Some(last + chrono::Duration::from_std(*d).ok()?),
            Schedule::Cron(c) => c
                .next_after(last.with_timezone(&Local))
                .map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// `300s` reads better as `5m` in a table; whole units only, else seconds.
pub fn describe_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s > 0 && s.is_multiple_of(86400) {
        format!("{}d", s / 86400)
    } else if s > 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s > 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// `minute hour day-of-month month day-of-week`. Each field: `*`, `n`, `a-b`,
/// `a-b/s`, `*/s`, or a comma list of those. Sunday is 0 or 7. Names are not
/// accepted. Day matching follows Vixie cron: if both day fields are restricted
/// a day matches when either does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    source: String,
    minute: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

/// The most days each month can have, indexed by month number. February
/// counts its leap day, since `0 0 29 2 *` does run, every four years.
const MONTH_DAYS: [usize; 13] = [0, 31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

impl CronExpr {
    pub fn parse(text: &str) -> Result<CronExpr, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron {text:?}: expected 5 fields (minute hour day month weekday), got {}",
                fields.len()
            ));
        }
        let (minute, _) = parse_field(fields[0], 0, 59).map_err(|e| format!("cron minute {e}"))?;
        let (hour, _) = parse_field(fields[1], 0, 23).map_err(|e| format!("cron hour {e}"))?;
        let (dom, dom_any) = parse_field(fields[2], 1, 31).map_err(|e| format!("cron day {e}"))?;
        let (month, _) = parse_field(fields[3], 1, 12).map_err(|e| format!("cron month {e}"))?;
        let (mut dow, dow_any) =
            parse_field(fields[4], 0, 7).map_err(|e| format!("cron weekday {e}"))?;
        if dow[7] {
            dow[0] = true;
        }
        dow.truncate(7);
        // With only the day of month restricted, some chosen month must have
        // one of the chosen days, or the job loads and never runs (`0 0 30 2
        // *`). With the weekday restricted too, Vixie's OR rule lets the
        // weekday match, so it always fires eventually.
        if !dom_any && dow_any && !(1..=12).any(|m| month[m] && (1..=MONTH_DAYS[m]).any(|d| dom[d]))
        {
            return Err(format!(
                "cron {text:?}: none of the chosen months has any of the chosen days, so it never runs"
            ));
        }
        Ok(CronExpr {
            source: fields.join(" "),
            minute,
            hour,
            dom,
            month,
            dow,
            dom_any,
            dow_any,
        })
    }

    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        self.next_after_in(after)
    }

    /// Walk forward in the zone's wall-clock time, one unit at a time, skipping
    /// whole months, days and hours that cannot match, so a once-a-year
    /// expression is found in a few thousand steps rather than half a million.
    /// A wall-clock minute that does not exist (spring forward) is skipped; an
    /// ambiguous one (fall back) fires at its earlier pass, unless `after` is
    /// itself inside that same fold and already past the earlier pass, in
    /// which case it fires at the later one — strictly-after holds either way.
    pub fn next_after_in<Tz: TimeZone>(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = after.timezone();
        let mut t =
            after.naive_local().with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
        // 29 February is the sparsest date a 5-field expression can name, and
        // it can be up to 8 years between leap years (2100 is not one), so
        // bound the walk there rather than at a round number that quietly
        // misses it.
        let limit = t + chrono::Duration::days(366 * 8);
        while t < limit {
            if !self.month[t.month() as usize] {
                t = start_of_next_month(t);
                continue;
            }
            if !self.day_matches(t.date()) {
                t = (t.date() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hour[t.hour() as usize] {
                t = t.with_minute(0)? + chrono::Duration::hours(1);
                continue;
            }
            if !self.minute[t.minute() as usize] {
                t += chrono::Duration::minutes(1);
                continue;
            }
            // Ambiguous (fall-back) resolves to whichever pass is actually
            // later than `after`: the naive walk can land back on a wall-clock
            // minute that, read on its first (earlier) pass, is before
            // `after`'s own (second-pass) instant, which would break
            // strictly-after.
            match tz.from_local_datetime(&t) {
                LocalResult::Single(dt) if dt > after => return Some(dt),
                LocalResult::Ambiguous(earliest, latest) => {
                    if earliest > after {
                        return Some(earliest);
                    }
                    if latest > after {
                        return Some(latest);
                    }
                }
                _ => {}
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let dom = self.dom[d.day() as usize];
        let dow = self.dow[d.weekday().num_days_from_sunday() as usize];
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        }
    }
}

fn start_of_next_month(t: NaiveDateTime) -> NaiveDateTime {
    let (y, m) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .expect("month 1..=12 always exists")
        .and_hms_opt(0, 0, 0)
        .expect("midnight always exists")
}

/// One field into a membership table over `min..=max`, plus whether it was
/// unrestricted (`*` or `*/n`), which the day-matching rule needs.
fn parse_field(text: &str, min: u32, max: u32) -> Result<(Vec<bool>, bool), String> {
    if text.is_empty() {
        return Err("field is empty".into());
    }
    let mut set = vec![false; max as usize + 1];
    let any = text.starts_with('*');
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                Some(parse_u32_strict(s).map_err(|_| format!("{part:?}: bad step"))?),
            ),
            None => (part, None),
        };
        if step == Some(0) {
            return Err(format!("{part:?}: step must be at least 1"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (parse_num(a, min, max)?, parse_num(b, min, max)?)
        } else {
            let n = parse_num(range, min, max)?;
            (n, if step.is_some() { max } else { n })
        };
        if lo > hi {
            return Err(format!("{part:?}: range runs backwards"));
        }
        // A step this side of the span can only ever mark `lo` and quickly
        // exceed `hi`; a step past it (e.g. a typo'd huge number) would still
        // do that on the first add, but bounding it here keeps `v += step`
        // nowhere near u32's ceiling so it can never overflow.
        if let Some(step) = step
            && step > hi - lo + 1
        {
            return Err(format!("{part:?}: step must be at most {}", hi - lo + 1));
        }
        let mut v = lo;
        while v <= hi {
            set[v as usize] = true;
            v += step.unwrap_or(1);
        }
    }
    Ok((set, any))
}

fn parse_num(s: &str, min: u32, max: u32) -> Result<u32, String> {
    let n = parse_u32_strict(s)
        .map_err(|_| format!("{s:?}: expected a number between {min} and {max}"))?;
    if n < min || n > max {
        return Err(format!("{s:?}: must be between {min} and {max}"));
    }
    Ok(n)
}

/// `u32::from_str` accepts a leading `+` (`"+5"` parses as `5`), which would
/// let a plus sign slip past the digits-only cron grammar; require plain
/// ASCII digits.
fn parse_u32_strict(s: &str) -> Result<u32, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{s:?}: expected digits"));
    }
    s.parse().map_err(|_| format!("{s:?}: number too large"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn a_cron_whose_days_never_fall_in_its_months_is_rejected() {
        for bad in ["0 0 30 2 *", "0 0 31 4,6,9,11 *", "0 0 31 2-4/2 *"] {
            let err = CronExpr::parse(bad).unwrap_err();
            assert!(err.contains("never runs"), "{bad}: {err}");
        }
        let err = Schedule::from_fields(None, Some("0 0 30 2 *")).unwrap_err();
        assert!(err.contains("never runs"), "{err}");
        // 29 February exists every four years; a weekday makes Vixie's OR
        // rule apply; a wildcard day always fits.
        for good in [
            "0 0 29 2 *",
            "0 0 30 2 1",
            "0 0 31 * *",
            "0 0 31 1-12 *",
            "0 0 * 2 *",
        ] {
            CronExpr::parse(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn every_is_last_plus_interval() {
        let s = Schedule::from_fields(Some("5m"), None).unwrap();
        assert_eq!(s, Schedule::Every(Duration::from_secs(300)));
        assert_eq!(
            s.next_after(utc("2026-09-24T10:00:00Z")),
            Some(utc("2026-09-24T10:05:00Z"))
        );
        assert_eq!(s.describe(), "every 5m");
        assert_eq!(
            Schedule::from_fields(Some("1d"), None).unwrap().describe(),
            "every 1d"
        );
    }

    #[test]
    fn exactly_one_of_every_and_cron() {
        assert!(
            Schedule::from_fields(None, None)
                .unwrap_err()
                .contains("every or cron")
        );
        assert!(
            Schedule::from_fields(Some("5m"), Some("* * * * *"))
                .unwrap_err()
                .contains("not both")
        );
        assert!(
            Schedule::from_fields(Some("0s"), None)
                .unwrap_err()
                .contains("zero")
        );
        assert!(Schedule::from_fields(Some("soon"), None).is_err());
    }

    // 2026-09-26 is a Saturday, 2026-09-28 a Monday.
    #[test]
    fn weekday_office_hours() {
        let c = CronExpr::parse("*/5 9-18 * * 1-5").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-26T10:03:00Z")),
            Some(utc("2026-09-28T09:00:00Z")),
            "Saturday rolls to Monday morning"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T09:00:00Z")),
            Some(utc("2026-09-28T09:05:00Z")),
            "strictly after: a run at 09:00 is not due again at 09:00"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T18:57:30Z")),
            Some(utc("2026-09-29T09:00:00Z"))
        );
        assert_eq!(Schedule::Cron(c).describe(), "cron */5 9-18 * * 1-5");
    }

    #[test]
    fn leap_day_is_found_across_years() {
        let c = CronExpr::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-03-01T00:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );
    }

    /// Vixie semantics: with both day-of-month and day-of-week restricted, a
    /// day matches either.
    #[test]
    fn day_of_month_or_day_of_week_when_both_are_set() {
        let c = CronExpr::parse("0 12 1 * 1").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T13:00:00Z")),
            Some(utc("2026-09-28T12:00:00Z")),
            "the Monday comes before the 1st"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T12:00:00Z")),
            Some(utc("2026-10-01T12:00:00Z")),
            "then the 1st, a Thursday"
        );
    }

    #[test]
    fn seven_is_sunday_and_lists_and_steps_parse() {
        let c = CronExpr::parse("0 0 * * 7").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T01:00:00Z")),
            Some(utc("2026-09-27T00:00:00Z"))
        );
        let c = CronExpr::parse("15,45 8-10/2 * 1,6 *").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T01:00:00Z")),
            Some(utc("2027-01-01T08:15:00Z"))
        );
        assert_eq!(
            c.next_after_in(utc("2027-01-01T08:15:00Z")),
            Some(utc("2027-01-01T08:45:00Z"))
        );
        assert_eq!(
            c.next_after_in(utc("2027-01-01T08:45:00Z")),
            Some(utc("2027-01-01T10:15:00Z"))
        );
    }

    #[test]
    fn rejects_malformed_expressions() {
        for bad in [
            "60 * * * *",
            "* 24 * * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "* * * *",
            "* * * * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            // u32::from_str accepts a leading `+`; the grammar does not.
            "+5 * * * *",
            "",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn describes_durations_in_their_largest_whole_unit() {
        assert_eq!(describe_duration(Duration::from_secs(45)), "45s");
        assert_eq!(describe_duration(Duration::from_secs(300)), "5m");
        assert_eq!(describe_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(describe_duration(Duration::from_secs(90)), "90s");
        assert_eq!(describe_duration(Duration::from_secs(172800)), "2d");
    }

    #[test]
    fn huge_steps_are_rejected_but_a_step_equal_to_the_span_still_parses() {
        assert!(
            CronExpr::parse("*/4294967295 * * * *").is_err(),
            "a step this large would overflow the u32 walk in parse_field"
        );
        assert!(
            CronExpr::parse("*/60 * * * *").is_ok(),
            "60 values, step 60: still just fires on minute 0, same as before"
        );
    }

    /// A time zone whose offset drops from +02:00 to +01:00 at a fixed UTC
    /// instant, so the wall-clock hour 01:00-02:00 on 2026-01-01 happens
    /// twice - a "fall back" fold, reproduced without depending on the
    /// host's real zone database (which no test can pin to a chosen instant).
    #[derive(Clone, Copy, Debug)]
    struct FoldingZone;

    impl FoldingZone {
        fn transition() -> NaiveDateTime {
            NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        }

        fn before() -> FixedOffset {
            FixedOffset::east_opt(2 * 3600).unwrap()
        }

        fn after() -> FixedOffset {
            FixedOffset::east_opt(3600).unwrap()
        }
    }

    impl TimeZone for FoldingZone {
        type Offset = FixedOffset;

        fn from_offset(_offset: &FixedOffset) -> Self {
            FoldingZone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            let fold_start = Self::transition() + chrono::Duration::hours(1);
            let fold_end = Self::transition() + chrono::Duration::hours(2);
            if *local < fold_start {
                LocalResult::Single(Self::before())
            } else if *local < fold_end {
                LocalResult::Ambiguous(Self::before(), Self::after())
            } else {
                LocalResult::Single(Self::after())
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if *utc < Self::transition() {
                Self::before()
            } else {
                Self::after()
            }
        }
    }

    #[test]
    fn strictly_after_holds_across_a_fall_back_fold() {
        let after = FoldingZone.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(0, 15, 0)
                .unwrap(),
        );
        let c = CronExpr::parse("*/5 * * * *").unwrap();
        let next = c.next_after_in(after).unwrap();
        assert!(
            next > after,
            "next_after_in must be strictly after `after`, got {next:?} for after={after:?}"
        );
        assert_eq!(
            next,
            FoldingZone.from_utc_datetime(
                &NaiveDate::from_ymd_opt(2026, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 20, 0)
                    .unwrap()
            ),
            "local 01:20 on its second pass, not its first (which is before `after`)"
        );
    }
}
