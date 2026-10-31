//! Отчёт о разрывах по записанным файлам: время без соединения, время
//! рассинхронизации стаканов и «тишина» рекордера. Критерий M0: < 0.1% времени разрывов.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::Serialize;
use sh_core::{RawSource, SysEvent};

use crate::layout::RecordLine;
use crate::reader::read_file;

/// Отчёт по одной площадке.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct VenueGapReport {
    /// Первая запись, нс.
    pub first_wall_ns: i64,
    /// Последняя запись, нс.
    pub last_wall_ns: i64,
    /// Записей по источникам.
    pub records_by_source: BTreeMap<String, u64>,
    /// Число соединений.
    pub connections: usize,
    /// Суммарное время без соединения по всем соединениям, с.
    pub conn_down_secs: f64,
    /// Доля времени без соединения: `conn_down / (connections * span)`.
    pub conn_gap_fraction: f64,
    /// Число стаканов, синхронизированных хотя бы раз.
    pub books: usize,
    /// Обнаружено разрывов последовательности.
    pub book_gaps: u64,
    /// Суммарное время рассинхронизации стаканов (разрывы и обрывы соединений), с.
    pub book_desync_secs: f64,
    /// Доля времени рассинхронизации: `desync / (books * span)`.
    pub book_gap_fraction: f64,
    /// Самая длинная пауза между соседними записями, с (простой рекордера).
    pub max_silence_secs: f64,
    /// Сумма пауз длиннее 10 с, с.
    pub silence_over_10s_secs: f64,
    /// Ошибок чтения (например, обрезанный хвост после аварии).
    pub read_errors: u64,
}

/// Отчёт по всем площадкам.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct GapReport {
    /// По площадкам.
    pub venues: BTreeMap<String, VenueGapReport>,
}

#[derive(Default)]
struct VenueState {
    rep: VenueGapReport,
    last_ns: Option<i64>,
    conn_down_since: HashMap<String, Option<i64>>,
    book_conn: HashMap<String, String>,
    seen_books: std::collections::HashSet<String>,
    desync_since: HashMap<String, i64>,
}

impl VenueState {
    fn observe(&mut self, ns: i64) {
        if self.rep.first_wall_ns == 0 || ns < self.rep.first_wall_ns {
            self.rep.first_wall_ns = ns;
        }
        if let Some(prev) = self.last_ns {
            let gap = (ns - prev) as f64 / 1e9;
            if gap > self.rep.max_silence_secs {
                self.rep.max_silence_secs = gap;
            }
            if gap > 10.0 {
                self.rep.silence_over_10s_secs += gap;
            }
        }
        self.last_ns = Some(self.last_ns.map_or(ns, |p| p.max(ns)));
        self.rep.last_wall_ns = self.rep.last_wall_ns.max(ns);
    }

    fn sys(&mut self, conn: &str, ns: i64, ev: SysEvent) {
        match ev {
            SysEvent::ConnUp { .. } => {
                if let Some(Some(since)) = self.conn_down_since.insert(conn.to_owned(), None) {
                    self.rep.conn_down_secs += (ns - since) as f64 / 1e9;
                }
            }
            SysEvent::ConnDown { reason } if reason == "shutdown" => {
                // штатная остановка процесса — не разрыв; простой рекордера виден как «тишина»
                self.conn_down_since.insert(conn.to_owned(), None);
                self.book_conn.retain(|_, c| c != conn);
            }
            SysEvent::ConnDown { .. } => {
                self.conn_down_since.insert(conn.to_owned(), Some(ns));
                // все стаканы соединения теряют синхронизацию
                for (book, c) in &self.book_conn {
                    if c == conn {
                        self.desync_since.entry(book.clone()).or_insert(ns);
                    }
                }
            }
            SysEvent::BookGap { symbol, .. } => {
                self.rep.book_gaps += 1;
                self.desync_since.entry(symbol).or_insert(ns);
            }
            SysEvent::BookSynced { symbol, .. } => {
                if let Some(since) = self.desync_since.remove(&symbol) {
                    self.rep.book_desync_secs += (ns - since) as f64 / 1e9;
                }
                self.seen_books.insert(symbol.clone());
                self.book_conn.insert(symbol, conn.to_owned());
            }
            SysEvent::Uncovered { .. } | SysEvent::RestError { .. } => {}
        }
    }

    fn finish(mut self) -> VenueGapReport {
        let end = self.rep.last_wall_ns;
        // незакрытые интервалы считаем до конца записи
        for since in self.conn_down_since.values().flatten() {
            self.rep.conn_down_secs += (end - since) as f64 / 1e9;
        }
        for since in self.desync_since.values() {
            self.rep.book_desync_secs += (end - since) as f64 / 1e9;
        }
        let span = ((end - self.rep.first_wall_ns) as f64 / 1e9).max(1e-9);
        self.rep.connections = self.conn_down_since.len();
        self.rep.books = self.seen_books.len();
        if self.rep.connections > 0 {
            self.rep.conn_gap_fraction = self.rep.conn_down_secs / (self.rep.connections as f64 * span);
        }
        if self.rep.books > 0 {
            self.rep.book_gap_fraction = self.rep.book_desync_secs / (self.rep.books as f64 * span);
        }
        self.rep
    }
}

/// Строит отчёт по файлам. Файлы обрабатываются в отсортированном по имени порядке
/// (имена содержат час UTC).
pub fn inspect_files(paths: &[PathBuf]) -> std::io::Result<GapReport> {
    let mut paths = paths.to_vec();
    paths.sort();
    let mut states: BTreeMap<String, VenueState> = BTreeMap::new();
    for p in &paths {
        for line in read_file(p)? {
            let rec = match line {
                Ok(RecordLine::Record(r)) => r,
                Ok(RecordLine::Header { .. }) => continue,
                Err(_) => {
                    // ошибку относим к площадке из имени каталога
                    let venue = p.parent().and_then(|d| d.parent()).and_then(|d| d.file_name());
                    let venue = venue.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
                    states.entry(venue).or_default().rep.read_errors += 1;
                    continue;
                }
            };
            let st = states.entry(rec.exchange.as_str().to_owned()).or_default();
            st.observe(rec.recv.wall_ns);
            let src = match &rec.source {
                RawSource::Ws => "ws",
                RawSource::Rest { .. } => "rest",
                RawSource::Sys => "sys",
                RawSource::Out => "out",
            };
            *st.rep.records_by_source.entry(src.to_owned()).or_default() += 1;
            if rec.source == RawSource::Sys
                && let Ok(ev) = serde_json::from_str::<SysEvent>(&rec.payload)
            {
                st.sys(&rec.conn, rec.recv.wall_ns, ev);
            }
        }
    }
    Ok(GapReport { venues: states.into_iter().map(|(k, v)| (k, v.finish())).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FORMAT, FileHeader};
    use crate::recorder::{HourlyWriter, RecorderConfig};
    use sh_core::{Exchange, RawRecord, RecvTs};

    const S: i64 = 1_000_000_000;

    fn sys(conn: &str, t: i64, ev: SysEvent) -> RawRecord {
        RawRecord {
            exchange: Exchange::Lighter,
            source: RawSource::Sys,
            conn: conn.into(),
            recv: RecvTs { wall_ns: t, mono_ns: 0 },
            payload: ev.to_json(),
        }
    }

    fn ws(t: i64) -> RawRecord {
        RawRecord {
            exchange: Exchange::Lighter,
            source: RawSource::Ws,
            conn: "c0".into(),
            recv: RecvTs { wall_ns: t, mono_ns: 0 },
            payload: "{}".into(),
        }
    }

    #[test]
    fn computes_gap_fractions() {
        let dir = std::env::temp_dir().join(format!("sh-inspect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = RecorderConfig { dir: dir.clone(), ..RecorderConfig::default() };
        let hdr = FileHeader {
            format: FORMAT.into(),
            session: "s".into(),
            host: "h".into(),
            session_start_wall_ns: 0,
            version: "0".into(),
        };
        let mut w = HourlyWriter::new(cfg, hdr);
        let t0 = 1_793_253_600 * S;
        let up = || SysEvent::ConnUp { url: "wss://x".into() };
        let synced = |s: &str| SysEvent::BookSynced { symbol: s.into(), seq: Some(1) };
        for r in [
            sys("c0", t0, up()),
            sys("c0", t0 + S, synced("A")),
            sys("c0", t0 + S, synced("B")),
            ws(t0 + 2 * S),
            // разрыв стакана A на 10 с
            sys("c0", t0 + 100 * S, SysEvent::BookGap { symbol: "A".into(), expected: 1, got: 3 }),
            sys("c0", t0 + 110 * S, synced("A")),
            // обрыв соединения на 5 с, оба стакана восстанавливаются через 6 с
            sys("c0", t0 + 200 * S, SysEvent::ConnDown { reason: "x".into() }),
            sys("c0", t0 + 205 * S, up()),
            sys("c0", t0 + 206 * S, synced("A")),
            sys("c0", t0 + 206 * S, synced("B")),
            ws(t0 + 1000 * S),
            // штатная остановка в конце записи не считается разрывом
            sys("c0", t0 + 1000 * S, SysEvent::ConnDown { reason: "shutdown".into() }),
        ] {
            w.push(&r).unwrap();
        }
        w.close_all().unwrap();
        let files = vec![crate::layout::hour_path(&dir, Exchange::Lighter, 1_793_253_600)];
        let rep = inspect_files(&files).unwrap();
        let v = &rep.venues["lighter"];
        assert_eq!(v.books, 2);
        assert_eq!(v.book_gaps, 1);
        assert!((v.book_desync_secs - 22.0).abs() < 1e-6);
        assert!((v.conn_down_secs - 5.0).abs() < 1e-6);
        assert_eq!(v.connections, 1);
        assert!((v.conn_gap_fraction - 5.0 / 1000.0).abs() < 1e-9);
        assert!((v.book_gap_fraction - 22.0 / 2000.0).abs() < 1e-9);
        assert!((v.max_silence_secs - 794.0).abs() < 1e-6);
        assert_eq!(v.records_by_source["ws"], 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
