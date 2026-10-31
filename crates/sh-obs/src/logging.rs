//! Логи: JSON в stdout (journald) и, если задан каталог, в файл с ежедневной ротацией по UTC.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Настройки `[log]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// Фильтр в синтаксисе `EnvFilter` (переопределяется `RUST_LOG`).
    pub filter: String,
    /// JSON в stdout (иначе человекочитаемый формат).
    pub json_stdout: bool,
    /// Каталог файлов логов; `None` — без файлов.
    pub dir: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { filter: "info".into(), json_stdout: true, dir: Some(PathBuf::from("data/logs")) }
    }
}

/// Ошибка инициализации логирования.
#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    /// Не удалось создать каталог.
    #[error("log dir: {0}")]
    Io(#[from] std::io::Error),
    /// Подписчик уже установлен.
    #[error("tracing init: {0}")]
    Init(String),
}

/// Файл с ежедневной ротацией: `<dir>/spread-hunter.<YYYY-MM-DD>.log`.
#[derive(Clone)]
pub struct DailyFile {
    inner: Arc<Mutex<DailyState>>,
}

struct DailyState {
    dir: PathBuf,
    date: String,
    file: Option<File>,
}

impl DailyFile {
    /// Создаёт писатель; каталог создаётся при необходимости.
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { inner: Arc::new(Mutex::new(DailyState { dir, date: String::new(), file: None })) })
    }

    /// Путь файла для даты.
    pub fn path_for(dir: &std::path::Path, date: &str) -> PathBuf {
        dir.join(format!("spread-hunter.{date}.log"))
    }
}

/// Писатель одной записи лога.
pub struct DailyWriter {
    inner: Arc<Mutex<DailyState>>,
}

impl Write for DailyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        if st.file.is_none() || st.date != today {
            let path = DailyFile::path_for(&st.dir, &today);
            st.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
            st.date = today;
        }
        match st.file.as_mut() {
            Some(f) => f.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.file.as_mut().map_or(Ok(()), File::flush)
    }
}

impl<'a> MakeWriter<'a> for DailyFile {
    type Writer = DailyWriter;

    fn make_writer(&'a self) -> Self::Writer {
        DailyWriter { inner: self.inner.clone() }
    }
}

/// Устанавливает глобальный подписчик `tracing`.
pub fn init_logging(cfg: &LogConfig) -> Result<(), LogInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.filter));
    let stdout_json = cfg.json_stdout.then(|| tracing_subscriber::fmt::layer().json().with_current_span(false));
    let stdout_text = (!cfg.json_stdout).then(tracing_subscriber::fmt::layer);
    let file = match &cfg.dir {
        Some(dir) => Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(false)
                .with_ansi(false)
                .with_writer(DailyFile::new(dir.clone())?),
        ),
        None => None,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_json)
        .with(stdout_text)
        .with(file)
        .try_init()
        .map_err(|e| LogInitError::Init(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_file_writes_dated_file() {
        let dir = std::env::temp_dir().join(format!("sh-obs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = DailyFile::new(dir.clone()).unwrap();
        let mut w = f.make_writer();
        w.write_all(b"{\"msg\":\"hello\"}\n").unwrap();
        w.flush().unwrap();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let content = std::fs::read_to_string(DailyFile::path_for(&dir, &today)).unwrap();
        assert!(content.contains("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
