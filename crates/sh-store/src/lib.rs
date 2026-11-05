//! Хранилище M0: сырой рекордер рыночных данных.
//! Сообщения площадок пишутся без изменений в JSONL, сжатый zstd, отдельным
//! файлом на каждую площадку и каждый час UTC. SQLite — состояние и журнал;
//! Parquet — производные таблицы для анализа (M1).

pub mod db;
pub mod inspect;
pub mod layout;
pub mod parquet_out;
pub mod reader;
pub mod recorder;

pub use db::{Db, DbError, LedgerRow};
pub use inspect::{GapReport, VenueGapReport, inspect_files};
pub use layout::{FORMAT, FileHeader, RecordLine, hour_path};
pub use reader::{ReadError, read_file};
pub use recorder::{Recorder, RecorderConfig, RecorderStats};
