//! When a job is due. `every` is a plain interval; `cron` is 5-field cron in the
//! head's local time, matched by a small walker rather than a crate: the
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
            Schedule::Every(d) => last.checked_add_signed(chrono::Duration::from_std(*d).ok()?),
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
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

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

    /// An interval that fits `chrono::Duration` but not added to a date
    /// (about 260,000 years) is never due; it used to panic the scheduler.
    #[test]
    fn an_every_past_the_calendar_is_never_due() {
        let s = Schedule::from_fields(Some("8208878385025s"), None).unwrap();
        assert_eq!(s.next_after(utc("2013-12-31T12:09:35Z")), None);
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

    /// A time zone whose offset rises from +01:00 to +02:00 at 2026-03-29
    /// 01:00 UTC, so local wall-clock 02:00-03:00 that day never happens - a
    /// "spring forward" gap, the mirror of `FoldingZone`.
    #[derive(Clone, Copy, Debug)]
    struct GapZone;

    impl GapZone {
        fn transition() -> NaiveDateTime {
            NaiveDate::from_ymd_opt(2026, 3, 29)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap()
        }

        fn before() -> FixedOffset {
            FixedOffset::east_opt(3600).unwrap()
        }

        fn after() -> FixedOffset {
            FixedOffset::east_opt(2 * 3600).unwrap()
        }

        /// A wall-clock time in this zone; panics on one inside the gap.
        fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<GapZone> {
            GapZone
                .from_local_datetime(
                    &NaiveDate::from_ymd_opt(y, mo, d)
                        .unwrap()
                        .and_hms_opt(h, mi, 0)
                        .unwrap(),
                )
                .single()
                .unwrap()
        }
    }

    impl TimeZone for GapZone {
        type Offset = FixedOffset;

        fn from_offset(_offset: &FixedOffset) -> Self {
            GapZone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            let gap_start = Self::transition() + chrono::Duration::hours(1);
            let gap_end = Self::transition() + chrono::Duration::hours(2);
            if *local < gap_start {
                LocalResult::Single(Self::before())
            } else if *local < gap_end {
                LocalResult::None
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
    fn a_cron_minute_inside_a_spring_forward_gap_is_skipped() {
        // 02:30 does not exist on the gap day: no run that day, and the next
        // is 02:30 the day after, not 03:30 or 03:00 as a stand-in.
        let daily = CronExpr::parse("30 2 * * *").unwrap();
        assert_eq!(
            daily.next_after_in(GapZone::local(2026, 3, 29, 1, 0)),
            Some(GapZone::local(2026, 3, 30, 2, 30))
        );
        assert_eq!(
            daily.next_after_in(GapZone::local(2026, 3, 28, 2, 30)),
            Some(GapZone::local(2026, 3, 30, 2, 30)),
            "from the day before's run, the gap day is skipped whole"
        );

        // A quarter-hourly job fires at 01:45 and resumes at 03:00, which
        // is only 15 real minutes later.
        let quarterly = CronExpr::parse("*/15 * * * *").unwrap();
        let last_before = GapZone::local(2026, 3, 29, 1, 45);
        let resumed = quarterly.next_after_in(last_before).unwrap();
        assert_eq!(resumed, GapZone::local(2026, 3, 29, 3, 0));
        assert_eq!(resumed - last_before, chrono::Duration::minutes(15));

        // Every minute: `after` just before the gap gives the first real
        // minute after it.
        let minutely = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(
            minutely.next_after_in(GapZone::local(2026, 3, 29, 1, 59)),
            Some(GapZone::local(2026, 3, 29, 3, 0))
        );
        let hourly = CronExpr::parse("0 * * * *").unwrap();
        assert_eq!(
            hourly.next_after_in(GapZone::local(2026, 3, 29, 1, 59)),
            Some(GapZone::local(2026, 3, 29, 3, 0))
        );
    }

    #[test]
    fn every_is_unaffected_by_a_spring_forward_gap() {
        // `every` adds its interval in UTC, so an hourly job keeps a real
        // hour between runs even though local time reads 01:30 then 03:30.
        let s = Schedule::from_fields(Some("1h"), None).unwrap();
        let last = GapZone::local(2026, 3, 29, 1, 30);
        let next = s.next_after(last.with_timezone(&Utc)).unwrap();
        assert_eq!(next - last.with_timezone(&Utc), chrono::Duration::hours(1));
        assert_eq!(
            next.with_timezone(&GapZone),
            GapZone::local(2026, 3, 29, 3, 30)
        );
    }

    /// One comma-separated part of a cron field, rendered as text, with the
    /// values it stands for worked out here rather than by `parse_field`.
    fn cron_part(min: u32, max: u32) -> impl Strategy<Value = (String, Vec<u32>, bool)> {
        let span = max - min + 1;
        let stepped = |lo: u32, hi: u32, step: u32| (lo..=hi).step_by(step as usize).collect();
        prop_oneof![
            Just(("*".to_string(), (min..=max).collect(), true)),
            (1..=span).prop_map(move |s| (format!("*/{s}"), stepped(min, max, s), true)),
            (min..=max).prop_map(|n| (n.to_string(), vec![n], false)),
            (min..=max, min..=max).prop_map(|(a, b)| {
                let (a, b) = (a.min(b), a.max(b));
                (format!("{a}-{b}"), (a..=b).collect(), false)
            }),
            (min..=max, min..=max, any::<u32>()).prop_map(move |(a, b, s)| {
                let (a, b) = (a.min(b), a.max(b));
                let s = s % (b - a + 1) + 1;
                (format!("{a}-{b}/{s}"), stepped(a, b, s), false)
            }),
            (min..=max, any::<u32>()).prop_map(move |(n, s)| {
                let s = s % (max - n + 1) + 1;
                (format!("{n}/{s}"), stepped(n, max, s), false)
            }),
        ]
    }

    /// A whole field: its text, a membership table over `0..=max`, and
    /// whether it counts as unrestricted, which Vixie cron (and pastor)
    /// decide by a leading `*`.
    fn cron_field(min: u32, max: u32) -> impl Strategy<Value = (String, Vec<bool>, bool)> {
        proptest::collection::vec(cron_part(min, max), 1..4).prop_map(move |parts| {
            let mut set = vec![false; max as usize + 1];
            for v in parts.iter().flat_map(|p| p.1.iter()) {
                set[*v as usize] = true;
            }
            let text: Vec<&str> = parts.iter().map(|p| p.0.as_str()).collect();
            (text.join(","), set, parts[0].2)
        })
    }

    /// A valid expression's text and an independent model of what it
    /// matches.
    #[derive(Debug, Clone)]
    struct CronModel {
        text: String,
        minute: Vec<bool>,
        hour: Vec<bool>,
        dom: Vec<bool>,
        dom_any: bool,
        month: Vec<bool>,
        dow: Vec<bool>,
        dow_any: bool,
    }

    impl CronModel {
        fn matches(&self, t: NaiveDateTime) -> bool {
            self.minute[t.minute() as usize]
                && self.hour[t.hour() as usize]
                && self.month[t.month() as usize]
                && self.day_matches(t.date())
        }

        fn day_matches(&self, d: NaiveDate) -> bool {
            let dom = self.dom[d.day() as usize];
            let wd = d.weekday().num_days_from_sunday() as usize;
            // 7 is Sunday as well as 0.
            let dow = self.dow[wd] || (wd == 0 && self.dow[7]);
            match (self.dom_any, self.dow_any) {
                (false, false) => dom || dow,
                (false, true) => dom,
                (true, false) => dow,
                (true, true) => true,
            }
        }

        /// The first matching wall-clock minute strictly after `t`, found by
        /// stepping one minute at a time and skipping only whole days that
        /// cannot match, so it shares nothing with `next_after_in`'s walk.
        fn brute_next(&self, t: NaiveDateTime) -> NaiveDateTime {
            let mut m = t.with_second(0).unwrap().with_nanosecond(0).unwrap()
                + chrono::Duration::minutes(1);
            loop {
                if !(self.month[m.month() as usize] && self.day_matches(m.date())) {
                    m = (m.date() + chrono::Duration::days(1))
                        .and_hms_opt(0, 0, 0)
                        .unwrap();
                } else if self.matches(m) {
                    return m;
                } else {
                    m += chrono::Duration::minutes(1);
                }
            }
        }
    }

    fn cron_model() -> impl Strategy<Value = CronModel> {
        (
            cron_field(0, 59),
            cron_field(0, 23),
            cron_field(1, 31),
            cron_field(1, 12),
            cron_field(0, 7),
        )
            .prop_map(|(mi, h, dom, mo, dow)| CronModel {
                text: format!("{} {} {} {} {}", mi.0, h.0, dom.0, mo.0, dow.0),
                minute: mi.1,
                hour: h.1,
                dom: dom.1,
                dom_any: dom.2,
                month: mo.1,
                dow: dow.1,
                dow_any: dow.2,
            })
            // `0 0 30 2 *` is rejected as never running; the generator leaves
            // such expressions out rather than teaching the model that rule.
            .prop_filter("some chosen month has a chosen day", |c| {
                c.dom_any
                    || !c.dow_any
                    || (1..=12).any(|m| c.month[m] && (1..=MONTH_DAYS[m]).any(|d| c.dom[d]))
            })
    }

    /// Instants from 1970 to about 2200, not aligned to the minute.
    fn instant() -> impl Strategy<Value = DateTime<Utc>> {
        (0i64..7_258_118_400, 0u32..1_000_000_000)
            .prop_map(|(s, ns)| DateTime::from_timestamp(s, ns).unwrap())
    }

    /// UTC and fixed offsets from -14:00 to +14:00 in whole minutes. There
    /// is no zone database in the tree, and `Local` depends on the host, so
    /// spring-forward gaps and fall-back folds are left to the hand-built
    /// zones above; the documented gap skip is not something this model
    /// would agree with.
    fn offset() -> impl Strategy<Value = FixedOffset> {
        prop_oneof![
            Just(FixedOffset::east_opt(0).unwrap()),
            (-14 * 60..=14 * 60).prop_map(|m| FixedOffset::east_opt(m * 60).unwrap()),
        ]
    }

    proptest! {
        /// Any text parses or is an error; the cron alphabet reaches deeper
        /// into `parse_field` than arbitrary Unicode does.
        #[test]
        fn prop_cron_parse_never_panics(
            s in "\\PC*|[0-9*/,\\- ]{0,40}|([0-9*/,-]{1,12} ){4}[0-9*/,-]{1,12}"
        ) {
            let _ = CronExpr::parse(&s);
        }

        /// For a valid expression, the next run is strictly after `after`,
        /// falls on a minute every field allows, and is the first such
        /// minute: brute-force stepping finds nothing earlier.
        #[test]
        fn prop_cron_next_is_the_first_matching_minute_after(
            c in cron_model(),
            at in instant(),
            tz in offset(),
        ) {
            let expr = CronExpr::parse(&c.text)
                .map_err(|e| TestCaseError::fail(format!("{}: {e}", c.text)))?;
            let after = at.with_timezone(&tz);
            let next = expr.next_after_in(after);
            prop_assert!(next.is_some(), "{} after {after}: no next run", c.text);
            let next = next.unwrap();
            prop_assert!(next > after, "{} after {after}: got {next}", c.text);
            prop_assert_eq!(next.second(), 0);
            prop_assert_eq!(next.nanosecond(), 0);
            prop_assert!(c.matches(next.naive_local()), "{}: {next} does not match", c.text);
            prop_assert_eq!(next.naive_local(), c.brute_next(after.naive_local()), "{}", c.text);
        }

        /// `every` fires exactly one interval after the last run, whatever
        /// unit the interval is written in.
        #[test]
        fn prop_every_is_last_plus_interval(
            n in 1u64..=100_000,
            unit in proptest::sample::select(vec![("s", 1u64), ("m", 60), ("h", 3600), ("d", 86400)]),
            last in instant(),
        ) {
            let s = Schedule::from_fields(Some(&format!("{n}{}", unit.0)), None).unwrap();
            let d = chrono::Duration::seconds((n * unit.1) as i64);
            prop_assert_eq!(s.next_after(last), Some(last + d));
        }

        /// An interval too long to add to the last run is no next run, not a
        /// panic in the scheduler.
        #[test]
        fn prop_every_never_panics(
            // Most of `u64` is too big for `chrono::Duration` at all; the
            // middle band fits there but not added to a date.
            n in prop_oneof![any::<u64>(), 1_000_000_000u64..10_000_000_000_000_000],
            unit in proptest::sample::select(vec!["s", "m", "h", "d"]),
            last in instant(),
        ) {
            if let Ok(s) = Schedule::from_fields(Some(&format!("{n}{unit}")), None) {
                let _ = s.next_after(last);
            }
        }

        /// `describe_duration` inverts `parse_duration` on whole seconds, the
        /// only durations `parse_duration` makes. Sub-second parts would be
        /// dropped, so they are not generated: the pair is an inverse on
        /// whole seconds only.
        #[test]
        fn prop_describe_then_parse_is_identity(secs in any::<u64>()) {
            let d = Duration::from_secs(secs);
            prop_assert_eq!(parse_duration(&describe_duration(d)), Ok(d));
        }
    }
}
