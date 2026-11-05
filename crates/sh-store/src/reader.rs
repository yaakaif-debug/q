//! Чтение файлов рекордера (многокадровый zstd, строки JSON).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use thiserror::Error;

use crate::layout::RecordLine;

/// Ошибка чтения строки.
#[derive(Debug, Error)]
pub enum ReadError {
    /// Ошибка ввода-вывода или распаковки (например, обрезанный последний кадр).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Строка не разбирается.
    #[error("line {line}: {error}")]
    Json {
        /// Номер строки (с 1).
        line: u64,
        /// Ошибка разбора.
        error: serde_json::Error,
    },
}

/// Итератор по строкам файла. После ошибки ввода-вывода итерация заканчивается.
pub fn read_file(path: &Path) -> std::io::Result<impl Iterator<Item = Result<RecordLine, ReadError>> + use<>> {
    let file = File::open(path)?;
    // Decoder по умолчанию читает все кадры подряд
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut lines = BufReader::with_capacity(1 << 16, decoder).lines();
    let mut n = 0u64;
    let mut failed = false;
    Ok(std::iter::from_fn(move || {
        if failed {
            return None;
        }
        let line = lines.next()?;
        n += 1;
        Some(match line {
            Ok(l) => serde_json::from_str::<RecordLine>(&l).map_err(|error| ReadError::Json { line: n, error }),
            Err(e) => {
                failed = true;
                Err(ReadError::Io(e))
            }
        })
    }))
}
