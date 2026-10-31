//! Хранилище M0: сырой рекордер рыночных данных.
//! Сообщения площадок пишутся без изменений в JSONL, сжатый zstd, отдельным
//! файлом на каждую площадку и каждый час UTC. SQLite и Parquet — в следующих этапах.

pub mod inspect;
pub mod layout;
pub mod reader;
pub mod recorder;

pub use inspect::{GapReport, VenueGapReport, inspect_files};
pub use layout::{FORMAT, FileHeader, RecordLine, hour_path};
pub use reader::{ReadError, read_file};
pub use recorder::{Recorder, RecorderConfig, RecorderStats};
