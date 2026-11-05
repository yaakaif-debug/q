//! Рекордер: отдельный поток пишет кадры zstd, чтобы задачи tokio не блокировались на диске.
//! Кадр сбрасывается по размеру или по времени, поэтому при аварии теряется не больше
//! `flush_interval` данных, а уже записанные кадры читаются.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sh_core::{Exchange, RawRecord};
use tokio::sync::mpsc;

use crate::layout::{FileHeader, RecordLine, hour_path, hour_start_secs};

/// Настройки рекордера `[recorder]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RecorderConfig {
    /// Писать сырые записи (в любом режиме; для `record` обязательно).
    pub enabled: bool,
    /// Каталог сырых записей.
    pub dir: PathBuf,
    /// Уровень сжатия zstd.
    pub zstd_level: i32,
    /// Максимальная задержка записи кадра, мс.
    pub flush_interval_ms: u64,
    /// Размер несжатого буфера, после которого кадр пишется сразу.
    pub max_frame_bytes: usize,
    /// Период fsync открытых файлов, с.
    pub fsync_interval_secs: u64,
    /// Ёмкость канала записей.
    pub channel_capacity: usize,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::from("data/raw"),
            zstd_level: 3,
            flush_interval_ms: 1000,
            max_frame_bytes: 1 << 20,
            fsync_interval_secs: 30,
            channel_capacity: 65_536,
        }
    }
}

/// Итоги работы рекордера.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecorderStats {
    /// Записей по площадкам.
    pub records: BTreeMap<String, u64>,
    /// Несжатых байт.
    pub raw_bytes: u64,
    /// Сжатых байт.
    pub compressed_bytes: u64,
    /// Открытых за работу файлов.
    pub files_opened: u64,
    /// Ошибок записи.
    pub write_errors: u64,
}

struct OpenFile {
    hour: i64,
    path: PathBuf,
    file: File,
    buf: Vec<u8>,
    first_buffered: Option<Instant>,
}

/// Писатель файлов по часам (используется потоком рекордера; доступен для тестов).
pub struct HourlyWriter {
    cfg: RecorderConfig,
    header: FileHeader,
    files: BTreeMap<Exchange, OpenFile>,
    stats: RecorderStats,
    last_fsync: Instant,
}

impl HourlyWriter {
    /// Создаёт писатель.
    pub fn new(cfg: RecorderConfig, header: FileHeader) -> Self {
        Self { cfg, header, files: BTreeMap::new(), stats: RecorderStats::default(), last_fsync: Instant::now() }
    }

    /// Добавляет запись. Час файла только растёт: запоздавшая на миллисекунды запись
    /// прошлого часа попадает в текущий файл (метки времени внутри записи сохраняются).
    pub fn push(&mut self, rec: &RawRecord) -> std::io::Result<()> {
        let hour = hour_start_secs(rec.recv.wall_ns);
        let rotate = self.files.get(&rec.exchange).is_none_or(|f| hour > f.hour);
        if rotate {
            if let Some(old) = self.files.remove(&rec.exchange) {
                self.close_file(old)?;
            }
            let f = self.open_file(rec.exchange, hour)?;
            self.files.insert(rec.exchange, f);
        }
        let line = serde_json::to_vec(rec).map_err(std::io::Error::other)?;
        *self.stats.records.entry(rec.exchange.as_str().to_owned()).or_default() += 1;
        let max = self.cfg.max_frame_bytes;
        let level = self.cfg.zstd_level;
        if let Some(f) = self.files.get_mut(&rec.exchange) {
            f.buf.extend_from_slice(&line);
            f.buf.push(b'\n');
            f.first_buffered.get_or_insert_with(Instant::now);
            if f.buf.len() >= max {
                Self::write_frame(f, level, &mut self.stats)?;
            }
        }
        Ok(())
    }

    /// Сбрасывает кадры, ждущие дольше `flush_interval`, и периодически делает fsync.
    pub fn flush_due(&mut self) -> std::io::Result<()> {
        let interval = Duration::from_millis(self.cfg.flush_interval_ms);
        let level = self.cfg.zstd_level;
        for f in self.files.values_mut() {
            if f.first_buffered.is_some_and(|t| t.elapsed() >= interval) {
                Self::write_frame(f, level, &mut self.stats)?;
            }
        }
        if self.last_fsync.elapsed() >= Duration::from_secs(self.cfg.fsync_interval_secs) {
            self.last_fsync = Instant::now();
            for f in self.files.values() {
                f.file.sync_data()?;
            }
        }
        Ok(())
    }

    /// Сбрасывает и закрывает все файлы.
    pub fn close_all(&mut self) -> std::io::Result<()> {
        let files = std::mem::take(&mut self.files);
        for (_, f) in files {
            self.close_file(f)?;
        }
        Ok(())
    }

    /// Статистика.
    pub fn stats(&self) -> &RecorderStats {
        &self.stats
    }

    fn open_file(&mut self, exchange: Exchange, hour: i64) -> std::io::Result<OpenFile> {
        let path = hour_path(&self.cfg.dir, exchange, hour);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // дописываем: после перезапуска в течение часа файл продолжается новыми кадрами
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.stats.files_opened += 1;
        tracing::info!(path = %path.display(), "recorder file opened");
        let mut f =
            OpenFile { hour, path, file, buf: Vec::with_capacity(self.cfg.max_frame_bytes), first_buffered: None };
        let hdr =
            serde_json::to_vec(&RecordLine::Header { hdr: self.header.clone() }).map_err(std::io::Error::other)?;
        f.buf.extend_from_slice(&hdr);
        f.buf.push(b'\n');
        f.first_buffered = Some(Instant::now());
        Ok(f)
    }

    fn close_file(&mut self, mut f: OpenFile) -> std::io::Result<()> {
        Self::write_frame(&mut f, self.cfg.zstd_level, &mut self.stats)?;
        f.file.sync_all()?;
        tracing::info!(path = %f.path.display(), "recorder file closed");
        Ok(())
    }

    fn write_frame(f: &mut OpenFile, level: i32, stats: &mut RecorderStats) -> std::io::Result<()> {
        if f.buf.is_empty() {
            return Ok(());
        }
        let frame = zstd::bulk::compress(&f.buf, level)?;
        f.file.write_all(&frame)?;
        stats.raw_bytes += f.buf.len() as u64;
        stats.compressed_bytes += frame.len() as u64;
        metrics::counter!("recorder_raw_bytes_total").increment(f.buf.len() as u64);
        metrics::counter!("recorder_compressed_bytes_total").increment(frame.len() as u64);
        f.buf.clear();
        f.first_buffered = None;
        Ok(())
    }
}

/// Запущенный рекордер.
pub struct Recorder {
    tx: mpsc::Sender<RawRecord>,
    thread: std::thread::JoinHandle<RecorderStats>,
}

impl Recorder {
    /// Запускает поток записи. Должен вызываться внутри рантайма tokio.
    pub fn start(cfg: RecorderConfig, header: FileHeader) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        let (tx, mut rx) = mpsc::channel::<RawRecord>(cfg.channel_capacity.max(1));
        let handle = tokio::runtime::Handle::current();
        let tick = Duration::from_millis(cfg.flush_interval_ms.clamp(10, 1000) / 2 + 1);
        let thread = std::thread::Builder::new().name("recorder".into()).spawn(move || {
            let mut w = HourlyWriter::new(cfg, header);
            loop {
                // ожидание с тайм-аутом, чтобы сбрасывать кадры и при тишине
                let next = handle.block_on(async { tokio::time::timeout(tick, rx.recv()).await });
                let res = match next {
                    Ok(Some(rec)) => w.push(&rec),
                    Ok(None) => break,
                    Err(_) => Ok(()),
                };
                if let Err(e) = res.and_then(|()| w.flush_due()) {
                    w.stats.write_errors += 1;
                    metrics::counter!("recorder_write_errors_total").increment(1);
                    tracing::error!(error = %e, "recorder write failed");
                }
                metrics::gauge!("recorder_queue_depth").set((rx.max_capacity() - rx.capacity()) as f64);
            }
            if let Err(e) = w.close_all() {
                tracing::error!(error = %e, "recorder close failed");
            }
            w.stats.clone()
        })?;
        Ok(Self { tx, thread })
    }

    /// Канал для адаптеров.
    pub fn sender(&self) -> mpsc::Sender<RawRecord> {
        self.tx.clone()
    }

    /// Останавливает запись после того, как все отправители закрыты; возвращает статистику.
    pub fn finish(self) -> RecorderStats {
        drop(self.tx);
        self.thread.join().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FORMAT;
    use crate::reader::read_file;
    use sh_core::{RawSource, RecvTs};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sh-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn rec(ex: Exchange, wall_ns: i64, payload: &str) -> RawRecord {
        RawRecord {
            exchange: ex,
            source: RawSource::Ws,
            conn: "c0".into(),
            recv: RecvTs { wall_ns, mono_ns: 7 },
            payload: payload.into(),
        }
    }

    fn header() -> FileHeader {
        FileHeader {
            format: FORMAT.into(),
            session: "s1".into(),
            host: "test".into(),
            session_start_wall_ns: 0,
            version: "0".into(),
        }
    }

    #[test]
    fn rotates_per_hour_and_venue_and_roundtrips_exact_payloads() {
        let dir = tmpdir("rotate");
        let cfg = RecorderConfig { dir: dir.clone(), max_frame_bytes: 64, ..RecorderConfig::default() };
        let mut w = HourlyWriter::new(cfg, header());
        let h0 = 1_793_253_600_000_000_000i64; // 06:00Z
        // полезная нагрузка с экранированием и юникодом должна вернуться байт в байт
        let tricky = "{\"a\":\"x\\\"y\",\"b\":\"тест\",\"c\":1e-7}";
        w.push(&rec(Exchange::Lighter, h0 + 1, tricky)).unwrap();
        w.push(&rec(Exchange::Aster, h0 + 2, "{}")).unwrap();
        w.push(&rec(Exchange::Lighter, h0 + 3_600_000_000_000, "{\"next\":1}")).unwrap();
        // запоздавшая запись прошлого часа остаётся в текущем файле
        w.push(&rec(Exchange::Lighter, h0 + 5, "{\"late\":1}")).unwrap();
        w.close_all().unwrap();

        let f06 = hour_path(&dir, Exchange::Lighter, 1_793_253_600);
        let f07 = hour_path(&dir, Exchange::Lighter, 1_793_257_200);
        let a06 = hour_path(&dir, Exchange::Aster, 1_793_253_600);
        let lines06: Vec<RecordLine> = read_file(&f06).unwrap().map(Result::unwrap).collect();
        assert!(matches!(lines06[0], RecordLine::Header { .. }));
        assert_eq!(lines06.len(), 2);
        let RecordLine::Record(r) = &lines06[1] else { panic!() };
        assert_eq!(r.payload, tricky);
        assert_eq!(r.recv, RecvTs { wall_ns: h0 + 1, mono_ns: 7 });
        let lines07: Vec<RecordLine> = read_file(&f07).unwrap().map(Result::unwrap).collect();
        assert_eq!(lines07.len(), 3);
        assert_eq!(read_file(&a06).unwrap().count(), 2);
        assert_eq!(w.stats().records.get("lighter"), Some(&3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopening_appends_new_frames() {
        let dir = tmpdir("append");
        let cfg = RecorderConfig { dir: dir.clone(), ..RecorderConfig::default() };
        let h0 = 1_793_253_600_000_000_000i64;
        for i in 0..2 {
            let mut w = HourlyWriter::new(cfg.clone(), header());
            w.push(&rec(Exchange::Hyperliquid, h0 + i, "{}")).unwrap();
            w.close_all().unwrap();
        }
        let p = hour_path(&dir, Exchange::Hyperliquid, 1_793_253_600);
        let lines: Vec<RecordLine> = read_file(&p).unwrap().map(Result::unwrap).collect();
        // два заголовка (две сессии) и две записи
        assert_eq!(lines.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_tail_keeps_complete_frames() {
        let dir = tmpdir("trunc");
        let cfg = RecorderConfig { dir: dir.clone(), max_frame_bytes: 1, ..RecorderConfig::default() };
        let mut w = HourlyWriter::new(cfg, header());
        let h0 = 1_793_253_600_000_000_000i64;
        for i in 0..5 {
            w.push(&rec(Exchange::Aster, h0 + i, "{\"k\":1}")).unwrap();
        }
        w.close_all().unwrap();
        let p = hour_path(&dir, Exchange::Aster, 1_793_253_600);
        // имитируем аварию посреди последнего кадра
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() - 5]).unwrap();
        let results: Vec<_> = read_file(&p).unwrap().collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert!(ok >= 5, "complete frames must stay readable, got {ok}");
        assert!(results.last().unwrap().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recorder_thread_flushes_on_finish() {
        let dir = tmpdir("thread");
        let cfg = RecorderConfig { dir: dir.clone(), ..RecorderConfig::default() };
        let r = Recorder::start(cfg, header()).unwrap();
        let tx = r.sender();
        let now = 1_793_253_600_000_000_000i64;
        for i in 0..100 {
            tx.send(rec(Exchange::Lighter, now + i, "{}")).await.unwrap();
        }
        drop(tx);
        let stats = tokio::task::spawn_blocking(move || r.finish()).await.unwrap();
        assert_eq!(stats.records.get("lighter"), Some(&100));
        let p = hour_path(&dir, Exchange::Lighter, 1_793_253_600);
        assert_eq!(read_file(&p).unwrap().count(), 101);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
