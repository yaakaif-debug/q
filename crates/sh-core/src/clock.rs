//! Часы. Код стратегий и адаптеров не читает системное время напрямую,
//! а получает `Clock`, чтобы повторы были детерминированными.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Локальные метки времени получения сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecvTs {
    /// Настенное время UTC, наносекунды от эпохи Unix.
    pub wall_ns: i64,
    /// Монотонное время, наносекунды от якоря процесса (сравнимо только внутри сессии).
    pub mono_ns: u64,
}

/// Источник времени.
pub trait Clock: Send + Sync {
    /// Настенное время UTC в наносекундах от эпохи Unix.
    fn wall_ns(&self) -> i64;
    /// Монотонное время в наносекундах от якоря.
    fn mono_ns(&self) -> u64;

    /// Обе метки сразу; монотонную берём первой, чтобы она была ближе к моменту получения.
    fn recv_ts(&self) -> RecvTs {
        let mono_ns = self.mono_ns();
        RecvTs { wall_ns: self.wall_ns(), mono_ns }
    }

    /// Настенное время в миллисекундах.
    fn wall_ms(&self) -> i64 {
        self.wall_ns().div_euclid(1_000_000)
    }
}

/// Системные часы: `chrono::Utc` и `Instant` от момента создания.
#[derive(Debug)]
pub struct SystemClock {
    anchor: Instant,
}

impl SystemClock {
    /// Создаёт часы с якорем монотонного времени в текущий момент.
    pub fn new() -> Self {
        Self { anchor: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn wall_ns(&self) -> i64 {
        // timestamp_nanos_opt вернёт None только после 2262 года
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX)
    }

    fn mono_ns(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Ручные часы для тестов и повторов.
#[derive(Debug, Default)]
pub struct ManualClock {
    wall: AtomicI64,
    mono: AtomicU64,
}

impl ManualClock {
    /// Создаёт часы с заданным настенным временем (нс) и нулевым монотонным.
    pub fn new(wall_ns: i64) -> Self {
        Self { wall: AtomicI64::new(wall_ns), mono: AtomicU64::new(0) }
    }

    /// Сдвигает оба времени вперёд на `delta_ns`.
    pub fn advance_ns(&self, delta_ns: u64) {
        self.mono.fetch_add(delta_ns, Ordering::SeqCst);
        let delta = i64::try_from(delta_ns).unwrap_or(i64::MAX);
        self.wall.fetch_add(delta, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn wall_ns(&self) -> i64 {
        self.wall.load(Ordering::SeqCst)
    }

    fn mono_ns(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_both() {
        let c = ManualClock::new(1_000);
        c.advance_ns(500);
        assert_eq!(c.recv_ts(), RecvTs { wall_ns: 1_500, mono_ns: 500 });
        assert_eq!(c.wall_ms(), 0);
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.mono_ns();
        let b = c.mono_ns();
        assert!(b >= a);
        assert!(c.wall_ns() > 1_700_000_000_000_000_000);
    }
}
