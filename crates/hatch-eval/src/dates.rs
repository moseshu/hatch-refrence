//! Date templates for worlds whose tasks hinge on "today".
//!
//! A live model may know the real date from outside the prompt (training,
//! or a date the provider adds). A world frozen on a past date then gives
//! it two different "todays", and relative-date tasks ("tomorrow",
//! "next Monday") are scored against the wrong one. Worlds that test such
//! reasoning write dates as templates resolved against the real current
//! date, so the prompt's clock and the model's own sense of the date agree.
//!
//! Tokens, with a signed day offset from the base date:
//!
//! | Token | Example (base 2026-11-10, a Tuesday) |
//! |---|---|
//! | `{{date+1}}` | `2026-11-11` |
//! | `{{md+1}}` | `11 月 11 日` |
//! | `{{wd+2}}` | `周四` |
//! | `{{nextweek+0}}` | `2026-11-16` (Monday of next week; `+4` is Friday) |
//!
//! The base date is today in the world's time zone, or `HATCH_EVAL_TODAY`
//! (`YYYY-MM-DD`) when set, to replay a run exactly.

use chrono::{Datelike, Duration, NaiveDate};
use hatch_core::{Error, Result};

/// Environment variable that pins the base date (`YYYY-MM-DD`).
pub const TODAY_ENV: &str = "HATCH_EVAL_TODAY";

const WEEKDAYS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

/// Today in `tz`, or the date pinned by [`TODAY_ENV`].
pub fn base_date(tz: &chrono_tz::Tz) -> Result<NaiveDate> {
    match std::env::var(TODAY_ENV) {
        Ok(v) if !v.trim().is_empty() => NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d")
            .map_err(|e| Error::Invalid(format!("{TODAY_ENV}={v}: {e}"))),
        _ => Ok(hatch_core::time::now().with_timezone(tz).date_naive()),
    }
}

/// Monday of the week after `d`'s week.
pub fn next_monday(d: NaiveDate) -> NaiveDate {
    d + Duration::days(7 - i64::from(d.weekday().num_days_from_monday()))
}

/// Resolve every date token in `text` against `base`.
pub fn render(text: &str, base: NaiveDate) -> Result<String> {
    let re = regex::Regex::new(r"\{\{(date|md|wd|nextweek)([+-]\d+)\}\}").expect("static regex");
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for c in re.captures_iter(text) {
        let whole = c.get(0).expect("match");
        let n: i64 = c[2]
            .parse()
            .map_err(|_| Error::Invalid(format!("bad date offset in {}", whole.as_str())))?;
        let value = match &c[1] {
            "date" => (base + Duration::days(n)).format("%Y-%m-%d").to_string(),
            "md" => {
                let d = base + Duration::days(n);
                format!("{} 月 {} 日", d.month(), d.day())
            }
            "wd" => WEEKDAYS[(base + Duration::days(n)).weekday().num_days_from_monday() as usize].to_string(),
            _ => (next_monday(base) + Duration::days(n)).format("%Y-%m-%d").to_string(),
        };
        out.push_str(&text[last..whole.start()]);
        out.push_str(&value);
        last = whole.end();
    }
    out.push_str(&text[last..]);
    if out.contains("{{") {
        return Err(Error::Invalid(format!("unresolved date template near: {}", &out[out.find("{{").unwrap_or(0)..].chars().take(40).collect::<String>())));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_resolve_against_the_base() {
        let tue = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        assert_eq!(render("{{date+1}}", tue).unwrap(), "2026-11-11");
        assert_eq!(render("{{date-1}}", tue).unwrap(), "2026-11-09");
        assert_eq!(render("{{md+1}}", tue).unwrap(), "11 月 11 日");
        assert_eq!(render("约{{wd+2}}，就是明天{{wd+1}}", tue).unwrap(), "约周四，就是明天周三");
        assert_eq!(render("{{nextweek+0}}T10:00", tue).unwrap(), "2026-11-16T10:00");
        assert_eq!(render("{{nextweek+4}}", tue).unwrap(), "2026-11-20");
        assert_eq!(render("no tokens", tue).unwrap(), "no tokens");
        assert!(render("{{bogus+1}}", tue).is_err());
    }

    #[test]
    fn next_monday_from_every_weekday() {
        let mon = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        for i in 0..7 {
            assert_eq!(next_monday(mon + Duration::days(i)), NaiveDate::from_ymd_opt(2026, 11, 16).unwrap());
        }
    }
}
