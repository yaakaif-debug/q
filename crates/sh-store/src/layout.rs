//! Раскладка файлов и формат строк.
//!
//! Путь: `<root>/<exchange>/<YYYY-MM-DD>/<exchange>-<YYYY-MM-DD>T<HH>.jsonl.zst`.
//! Каждый файл — последовательность независимых кадров zstd; каждый кадр — целые строки.
//! Первая строка после каждого открытия файла — заголовок `{"hdr": {...}}`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sh_core::{Exchange, RawRecord};

/// Версия формата.
pub const FORMAT: &str = "sh-raw-v1";

/// Заголовок файла: сведения о сессии записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHeader {
    /// Версия формата.
    pub format: String,
    /// Идентификатор сессии (процесса) записи; монотонное время сравнимо только внутри сессии.
    pub session: String,
    /// Имя хоста.
    pub host: String,
    /// Настенное время старта сессии, нс.
    pub session_start_wall_ns: i64,
    /// Версия программы.
    pub version: String,
}

/// Строка файла: заголовок или запись.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecordLine {
    /// Заголовок.
    Header {
        /// Содержимое заголовка.
        hdr: FileHeader,
    },
    /// Сырая запись.
    Record(RawRecord),
}

/// Начало часа UTC (секунды от эпохи) для времени в наносекундах.
pub fn hour_start_secs(wall_ns: i64) -> i64 {
    wall_ns.div_euclid(1_000_000_000).div_euclid(3600) * 3600
}

/// Путь файла для площадки и часа.
pub fn hour_path(root: &Path, exchange: Exchange, hour_start_secs: i64) -> PathBuf {
    let dt: DateTime<Utc> = DateTime::from_timestamp(hour_start_secs, 0).unwrap_or_default();
    let ex = exchange.as_str();
    root.join(ex).join(dt.format("%Y-%m-%d").to_string()).join(format!("{ex}-{}.jsonl.zst", dt.format("%Y-%m-%dT%H")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_paths() {
        // 2026-10-29T06:30:00Z
        let ns = 1_793_255_400_000_000_000;
        let h = hour_start_secs(ns);
        assert_eq!(h, 1_793_253_600);
        let p = hour_path(Path::new("/data/raw"), Exchange::Lighter, h);
        assert_eq!(p, PathBuf::from("/data/raw/lighter/2026-10-29/lighter-2026-10-29T06.jsonl.zst"));
    }

    #[test]
    fn header_line_is_distinguishable() {
        let hdr = RecordLine::Header {
            hdr: FileHeader {
                format: FORMAT.into(),
                session: "s".into(),
                host: "h".into(),
                session_start_wall_ns: 1,
                version: "0".into(),
            },
        };
        let s = serde_json::to_string(&hdr).unwrap();
        assert!(s.starts_with("{\"hdr\":"));
        assert_eq!(serde_json::from_str::<RecordLine>(&s).unwrap(), hdr);
    }
}
