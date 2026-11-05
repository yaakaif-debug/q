//! SQLite (режим WAL) для состояния и журнала. Суммы хранятся текстом `Decimal`,
//! чтобы не терять точность. Схема мигрирует по номеру версии при открытии.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

/// Ошибка базы.
#[derive(Debug, Error)]
pub enum DbError {
    /// Ошибка SQLite.
    #[error("sqlite: {0}")]
    Sql(#[from] sqlx::Error),
    /// Ошибка файловой системы.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Миграции по порядку; индекс + 1 — версия схемы.
const MIGRATIONS: &[&str] = &[
    // 1: M1 — ключ-значение, журнал теневого портфеля, первое появление инструментов
    "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_ms INTEGER NOT NULL);
     CREATE TABLE shadow_ledger (id INTEGER PRIMARY KEY AUTOINCREMENT, ts_ms INTEGER NOT NULL, pair_id TEXT NOT NULL,
         leg TEXT, kind TEXT NOT NULL, amount TEXT NOT NULL);
     CREATE INDEX shadow_ledger_ts ON shadow_ledger(ts_ms);
     CREATE TABLE first_seen (instrument TEXT PRIMARY KEY, ts_ms INTEGER NOT NULL);",
];

/// Строка журнала.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// Время, мс.
    pub ts_ms: i64,
    /// Пара.
    pub pair_id: String,
    /// Нога.
    pub leg: Option<String>,
    /// Вид записи.
    pub kind: String,
    /// Сумма (текст `Decimal`).
    pub amount: String,
}

/// База состояния.
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Открывает или создаёт базу и применяет миграции.
    pub async fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new().max_connections(4).connect_with(opts).await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// База в памяти (тесты).
    pub async fn in_memory() -> Result<Self, DbError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?;
        // одно соединение: у каждого соединения к :memory: своя база
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// Пул соединений для расширений схемы в следующих этапах.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn migrate(&self) -> Result<(), DbError> {
        sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)").execute(&self.pool).await?;
        let current: i64 = sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version")
            .fetch_one(&self.pool)
            .await?
            .try_get("v")?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let version = i64::try_from(i).unwrap_or(i64::MAX) + 1;
            if version <= current {
                continue;
            }
            let mut tx = self.pool.begin().await?;
            sqlx::raw_sql(*sql).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO schema_version (version) VALUES (?)").bind(version).execute(&mut *tx).await?;
            tx.commit().await?;
            tracing::info!(version, "database migrated");
        }
        Ok(())
    }

    /// Текущая версия схемы.
    pub async fn schema_version(&self) -> Result<i64, DbError> {
        Ok(sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version")
            .fetch_one(&self.pool)
            .await?
            .try_get("v")?)
    }

    /// Записывает значение по ключу.
    pub async fn kv_put(&self, key: &str, value: &str, now_ms: i64) -> Result<(), DbError> {
        sqlx::query("INSERT INTO kv (key, value, updated_ms) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms")
            .bind(key)
            .bind(value)
            .bind(now_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Значение по ключу.
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT value FROM kv WHERE key = ?").bind(key).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.try_get("value")).transpose()?)
    }

    /// Добавляет строки журнала одной транзакцией.
    pub async fn append_ledger(&self, rows: &[LedgerRow]) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for r in rows {
            sqlx::query("INSERT INTO shadow_ledger (ts_ms, pair_id, leg, kind, amount) VALUES (?, ?, ?, ?, ?)")
                .bind(r.ts_ms)
                .bind(&r.pair_id)
                .bind(&r.leg)
                .bind(&r.kind)
                .bind(&r.amount)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Журнал за `[from_ms, to_ms)`.
    pub async fn ledger_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<LedgerRow>, DbError> {
        let rows = sqlx::query(
            "SELECT ts_ms, pair_id, leg, kind, amount FROM shadow_ledger WHERE ts_ms >= ? AND ts_ms < ? ORDER BY id",
        )
        .bind(from_ms)
        .bind(to_ms)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(LedgerRow {
                    ts_ms: r.try_get("ts_ms")?,
                    pair_id: r.try_get("pair_id")?,
                    leg: r.try_get("leg")?,
                    kind: r.try_get("kind")?,
                    amount: r.try_get("amount")?,
                })
            })
            .collect()
    }

    /// Запоминает время первого появления инструментов (существующие не меняются).
    pub async fn first_seen_insert(&self, instruments: &[String], now_ms: i64) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for i in instruments {
            sqlx::query("INSERT OR IGNORE INTO first_seen (instrument, ts_ms) VALUES (?, ?)")
                .bind(i)
                .bind(now_ms)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Все времена первого появления.
    pub async fn first_seen_all(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = sqlx::query("SELECT instrument, ts_ms FROM first_seen").fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok((r.try_get("instrument")?, r.try_get("ts_ms")?))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kv_ledger_first_seen() {
        let db = Db::in_memory().await.unwrap();
        assert_eq!(db.schema_version().await.unwrap(), 1);
        assert_eq!(db.kv_get("a").await.unwrap(), None);
        db.kv_put("a", "{\"x\":1}", 1).await.unwrap();
        db.kv_put("a", "{\"x\":2}", 2).await.unwrap();
        assert_eq!(db.kv_get("a").await.unwrap().as_deref(), Some("{\"x\":2}"));
        let row = |ts| LedgerRow {
            ts_ms: ts,
            pair_id: "p".into(),
            leg: None,
            kind: "fee".into(),
            amount: "-1.2345678901234567".into(),
        };
        db.append_ledger(&[row(10), row(20), row(30)]).await.unwrap();
        let got = db.ledger_between(10, 30).await.unwrap();
        assert_eq!(got.len(), 2);
        // точность Decimal сохраняется
        assert_eq!(got[0].amount, "-1.2345678901234567");
        db.first_seen_insert(&["lighter/ETH".into()], 100).await.unwrap();
        db.first_seen_insert(&["lighter/ETH".into(), "aster/X".into()], 200).await.unwrap();
        let fs = db.first_seen_all().await.unwrap();
        assert_eq!(fs.get("lighter/ETH"), Some(&100));
        assert_eq!(fs.get("aster/X"), Some(&200));
    }

    #[tokio::test]
    async fn file_db_reopens_without_remigrating() {
        let dir = std::env::temp_dir().join(format!("sh-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.sqlite");
        {
            let db = Db::open(&path).await.unwrap();
            db.kv_put("k", "v", 1).await.unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.schema_version().await.unwrap(), 1);
        assert_eq!(db.kv_get("k").await.unwrap().as_deref(), Some("v"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
