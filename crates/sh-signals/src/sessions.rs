//! Календари смены сессий базовых активов (акции и товары переоцениваются иначе
//! на выходных и при открытии рынка). Время — UTC; переход на летнее время оператор
//! учитывает в конфиге (часовых поясов в зависимостях нет).

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use thiserror::Error;

/// Ошибка разбора календаря.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("bad session change '{0}': expected 'MON 13:30'")]
pub struct CalendarError(pub String);

/// Недельный календарь моментов смены сессии.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Calendar {
    /// Минуты от начала недели (понедельник 00:00 UTC).
    changes: Vec<i64>,
}

const WEEK_MIN: i64 = 7 * 24 * 60;

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s.to_ascii_uppercase().as_str() {
        "MON" => Weekday::Mon,
        "TUE" => Weekday::Tue,
        "WED" => Weekday::Wed,
        "THU" => Weekday::Thu,
        "FRI" => Weekday::Fri,
        "SAT" => Weekday::Sat,
        "SUN" => Weekday::Sun,
        _ => return None,
    })
}

impl Calendar {
    /// Разбирает строки вида `MON 13:30`.
    pub fn parse(specs: &[String]) -> Result<Self, CalendarError> {
        let mut changes = Vec::with_capacity(specs.len());
        for s in specs {
            let bad = || CalendarError(s.clone());
            let (d, t) = s.trim().split_once(' ').ok_or_else(bad)?;
            let (h, m) = t.trim().split_once(':').ok_or_else(bad)?;
            let (h, m): (i64, i64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
            if !(0..24).contains(&h) || !(0..60).contains(&m) {
                return Err(bad());
            }
            let day = i64::from(weekday(d).ok_or_else(bad)?.num_days_from_monday());
            changes.push(day * 1440 + h * 60 + m);
        }
        changes.sort_unstable();
        Ok(Self { changes })
    }

    /// Минут до ближайшей смены сессии (в прошлом или будущем); `None` для пустого календаря.
    pub fn minutes_to_nearest_change(&self, now: DateTime<Utc>) -> Option<i64> {
        let now_min = i64::from(now.weekday().num_days_from_monday()) * 1440
            + i64::from(now.hour()) * 60
            + i64::from(now.minute());
        self.changes
            .iter()
            .map(|c| {
                let d = (c - now_min).rem_euclid(WEEK_MIN);
                d.min(WEEK_MIN - d)
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn nearest_change_wraps_week() {
        let c = Calendar::parse(&["MON 13:30".into(), "FRI 20:00".into()]).unwrap();
        // понедельник 13:00 → 30 минут
        let mon = Utc.with_ymd_and_hms(2026, 11, 2, 13, 0, 0).unwrap();
        assert_eq!(c.minutes_to_nearest_change(mon), Some(30));
        // воскресенье 23:50 → понедельник 13:30 далеко, пятница 20:00 ещё дальше; ближе понедельник
        let sun = Utc.with_ymd_and_hms(2026, 11, 8, 23, 50, 0).unwrap();
        assert_eq!(c.minutes_to_nearest_change(sun), Some(13 * 60 + 40));
        // пятница 20:20 → 20 минут после смены
        let fri = Utc.with_ymd_and_hms(2026, 11, 6, 20, 20, 0).unwrap();
        assert_eq!(c.minutes_to_nearest_change(fri), Some(20));
        assert_eq!(Calendar::default().minutes_to_nearest_change(fri), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Calendar::parse(&["XYZ 10:00".into()]).is_err());
        assert!(Calendar::parse(&["MON 25:00".into()]).is_err());
        assert!(Calendar::parse(&["MON1300".into()]).is_err());
    }
}
