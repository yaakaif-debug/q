//! Источник повтора: сырые записи всех площадок за интервал времени, слитые по
//! времени получения, и разбор их обратно в нормализованные события.

use std::iter::Peekable;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sh_core::{Exchange, RawRecord};
use sh_store::RecordLine;
use sh_venue_api::{MdEvent, Replay};
use sh_venue_aster::AsReplay;
use sh_venue_hyperliquid::HlReplay;
use sh_venue_lighter::LtReplay;

type RecordIter = Box<dyn Iterator<Item = RawRecord>>;

/// Файлы площадки, пересекающие интервал `[from_ms, to_ms)`, по возрастанию часа.
pub fn venue_files(root: &Path, exchange: Exchange, from_ms: i64, to_ms: i64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = root.join(exchange.as_str());
    let Ok(days) = std::fs::read_dir(&dir) else { return out };
    for day in days.flatten() {
        let Ok(files) = std::fs::read_dir(day.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
            // <venue>-<YYYY-MM-DD>T<HH>.jsonl.zst
            let Some(stamp) =
                name.strip_prefix(&format!("{}-", exchange.as_str())).and_then(|s| s.strip_suffix(".jsonl.zst"))
            else {
                continue;
            };
            let Ok(hour) = chrono::NaiveDateTime::parse_from_str(&format!("{stamp}:00:00"), "%Y-%m-%dT%H:%M:%S") else {
                continue;
            };
            let start = hour.and_utc().timestamp_millis();
            if start + 3_600_000 > from_ms && start < to_ms {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn file_records(files: Vec<PathBuf>, from_ms: i64, to_ms: i64) -> RecordIter {
    Box::new(
        files
            .into_iter()
            .flat_map(|p| match sh_store::read_file(&p) {
                Ok(it) => Box::new(it.filter_map(|l| match l {
                    Ok(RecordLine::Record(r)) => Some(r),
                    _ => None,
                })) as RecordIter,
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "cannot open recording");
                    Box::new(std::iter::empty())
                }
            })
            .filter(move |r| {
                let ms = r.recv.wall_ns.div_euclid(1_000_000);
                ms >= from_ms && ms < to_ms
            }),
    )
}

/// Слитый по времени поток записей нескольких площадок.
pub struct ReplaySource {
    streams: Vec<Peekable<RecordIter>>,
}

impl ReplaySource {
    /// Открывает записи площадок `venues` из `root` за `[from_ms, to_ms)`.
    pub fn open(root: &Path, venues: &[Exchange], from_ms: i64, to_ms: i64) -> Self {
        let streams = venues
            .iter()
            .map(|ex| file_records(venue_files(root, *ex, from_ms, to_ms), from_ms, to_ms).peekable())
            .collect();
        Self { streams }
    }

    /// Поток из готовых итераторов (тесты).
    pub fn from_iters(iters: Vec<RecordIter>) -> Self {
        Self { streams: iters.into_iter().map(Iterator::peekable).collect() }
    }
}

impl Iterator for ReplaySource {
    type Item = RawRecord;

    fn next(&mut self) -> Option<RawRecord> {
        // внутри площадки порядок файла сохраняется; между площадками — по времени получения
        let mut best: Option<(usize, i64)> = None;
        for (i, s) in self.streams.iter_mut().enumerate() {
            if let Some(r) = s.peek()
                && best.is_none_or(|(_, t)| r.recv.wall_ns < t)
            {
                best = Some((i, r.recv.wall_ns));
            }
        }
        self.streams[best?.0].next()
    }
}

/// Разборщики повтора по площадкам.
pub struct Replayers {
    hl: HlReplay,
    lt: LtReplay,
    ast: AsReplay,
}

impl Replayers {
    /// Новые разборщики.
    pub fn new() -> Self {
        Self { hl: HlReplay::new(), lt: LtReplay::new(), ast: AsReplay::new() }
    }

    /// События из записи.
    pub fn feed(&mut self, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        match rec.exchange {
            Exchange::Hyperliquid => self.hl.feed(rec, out),
            Exchange::Lighter => self.lt.feed(rec, out),
            Exchange::Aster => self.ast.feed(rec, out),
        }
    }
}

impl Default for Replayers {
    fn default() -> Self {
        Self::new()
    }
}

/// Разбор даты `YYYY-MM-DD` или RFC 3339 в миллисекунды UTC.
pub fn parse_time(s: &str) -> Result<i64, String> {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp_millis()).unwrap_or(0));
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| format!("bad time '{s}': use YYYY-MM-DD or RFC 3339"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sh_core::{RawSource, RecvTs};

    fn rec(ex: Exchange, ns: i64) -> RawRecord {
        RawRecord {
            exchange: ex,
            source: RawSource::Ws,
            conn: "c".into(),
            recv: RecvTs { wall_ns: ns, mono_ns: 0 },
            payload: "{}".into(),
        }
    }

    #[test]
    fn merges_by_receive_time() {
        let a: RecordIter = Box::new(vec![rec(Exchange::Aster, 1), rec(Exchange::Aster, 5)].into_iter());
        let b: RecordIter =
            Box::new(vec![rec(Exchange::Lighter, 2), rec(Exchange::Lighter, 3), rec(Exchange::Lighter, 9)].into_iter());
        let order: Vec<i64> = ReplaySource::from_iters(vec![a, b]).map(|r| r.recv.wall_ns).collect();
        assert_eq!(order, vec![1, 2, 3, 5, 9]);
    }

    #[test]
    fn times_and_files() {
        assert_eq!(parse_time("1970-01-02").unwrap(), 86_400_000);
        assert_eq!(parse_time("1970-01-01T00:00:01Z").unwrap(), 1000);
        assert!(parse_time("yesterday").is_err());
        let dir = std::env::temp_dir().join(format!("sh-replay-{}", std::process::id()));
        let d = dir.join("lighter/2026-10-29");
        std::fs::create_dir_all(&d).unwrap();
        for h in ["05", "06", "07"] {
            std::fs::write(d.join(format!("lighter-2026-10-29T{h}.jsonl.zst")), b"").unwrap();
        }
        let from = parse_time("2026-10-29T06:30:00Z").unwrap();
        let to = parse_time("2026-10-29T07:00:00Z").unwrap();
        let files = venue_files(&dir, Exchange::Lighter, from, to);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("lighter-2026-10-29T06.jsonl.zst"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
