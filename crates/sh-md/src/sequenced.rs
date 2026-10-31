//! Стакан с номерами последовательности: буферизация до снимка,
//! проверка непрерывности и сигнал на пересинхронизацию.
//!
//! Правила общие для площадок с дельтами:
//! * Lighter: `prev = begin_nonce`, `last = nonce`; `begin_nonce` должен равняться
//!   `nonce` предыдущего сообщения.
//! * Aster: `first = U`, `last = u`, `prev = pu`; первое событие после REST-снимка
//!   должно покрывать `lastUpdateId` (`U <= id <= u`), далее `pu` == предыдущий `u`.

use std::collections::VecDeque;

use sh_core::Level;

use crate::book::OrderBook;

/// Номера последовательности одного дельта-сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaSeq {
    /// Первый номер, покрытый сообщением.
    pub first: u64,
    /// Последний номер, покрытый сообщением.
    pub last: u64,
    /// Номер, которым должно было закончиться предыдущее сообщение, если площадка его сообщает.
    pub prev: Option<u64>,
}

/// Дельта стакана: абсолютные количества по уровням.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookDelta {
    /// Номера последовательности.
    pub seq: DeltaSeq,
    /// Обновления bids.
    pub bids: Vec<Level>,
    /// Обновления asks.
    pub asks: Vec<Level>,
}

/// Состояние синхронизации стакана.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// Ждём снимок; дельты буферизуются.
    AwaitingSnapshot,
    /// Снимок применён, дельт после него ещё не было.
    FreshSnapshot,
    /// Стакан непрерывен.
    Synced,
}

/// Результат применения сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Применено.
    Applied,
    /// Отложено до снимка.
    Buffered,
    /// Устаревшее сообщение (уже покрыто снимком), пропущено.
    Stale,
    /// Разрыв: стакан сброшен в ожидание снимка, нужна пересинхронизация.
    Gap {
        /// Ожидаемый номер (последний применённый).
        expected: u64,
        /// Полученный номер (`prev`, а если его нет — `first`).
        got: u64,
    },
}

/// Стакан с контролем последовательности.
#[derive(Debug, Clone)]
pub struct SequencedBook {
    book: OrderBook,
    state: SyncState,
    last_seq: u64,
    pending: VecDeque<BookDelta>,
    max_pending: usize,
}

impl SequencedBook {
    /// Новый стакан в ожидании снимка; `max_pending` ограничивает буфер дельт.
    pub fn new(max_pending: usize) -> Self {
        Self {
            book: OrderBook::new(),
            state: SyncState::AwaitingSnapshot,
            last_seq: 0,
            pending: VecDeque::new(),
            max_pending: max_pending.max(1),
        }
    }

    /// Текущий стакан (валиден только вне `AwaitingSnapshot`).
    pub fn book(&self) -> &OrderBook {
        &self.book
    }

    /// Изменяемый доступ к стакану (метки времени).
    pub fn book_mut(&mut self) -> &mut OrderBook {
        &mut self.book
    }

    /// Состояние синхронизации.
    pub fn state(&self) -> SyncState {
        self.state
    }

    /// Истина, если стакан можно использовать.
    pub fn is_synced(&self) -> bool {
        self.state != SyncState::AwaitingSnapshot
    }

    /// Последний применённый номер.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Сбрасывает стакан в ожидание снимка (разрыв соединения, ручная пересинхронизация).
    pub fn invalidate(&mut self) {
        self.book.clear();
        self.state = SyncState::AwaitingSnapshot;
        self.pending.clear();
    }

    /// Применяет снимок с номером `seq`, затем буферизованные дельты.
    /// Возвращает `Gap`, если буфер не стыкуется со снимком.
    pub fn apply_snapshot(&mut self, seq: u64, bids: &[Level], asks: &[Level]) -> ApplyOutcome {
        self.book.replace(bids, asks);
        self.last_seq = seq;
        self.state = SyncState::FreshSnapshot;
        // потеря старых дельт при переполнении буфера проявится как разрыв ниже
        let pending = std::mem::take(&mut self.pending);
        for delta in pending {
            if let gap @ ApplyOutcome::Gap { .. } = self.apply_synced(&delta) {
                return gap;
            }
        }
        ApplyOutcome::Applied
    }

    /// Применяет дельту с проверкой непрерывности.
    pub fn apply_delta(&mut self, delta: BookDelta) -> ApplyOutcome {
        if self.state == SyncState::AwaitingSnapshot {
            if self.pending.len() >= self.max_pending {
                // старейшие дельты теряем; если снимок окажется новее их, потери не будет
                self.pending.pop_front();
            }
            self.pending.push_back(delta);
            return ApplyOutcome::Buffered;
        }
        self.apply_synced(&delta)
    }

    fn apply_synced(&mut self, delta: &BookDelta) -> ApplyOutcome {
        let l = self.last_seq;
        let s = delta.seq;
        let accept = match self.state {
            SyncState::AwaitingSnapshot => false,
            SyncState::FreshSnapshot => {
                if s.last < l {
                    return ApplyOutcome::Stale;
                }
                s.prev == Some(l) || (s.first <= l.saturating_add(1) && s.last >= l)
            }
            SyncState::Synced => match s.prev {
                Some(p) => p == l,
                None => s.first == l.saturating_add(1),
            },
        };
        if accept {
            self.book.apply_levels(&delta.bids, &delta.asks);
            self.last_seq = s.last;
            self.state = SyncState::Synced;
            return ApplyOutcome::Applied;
        }
        if self.state == SyncState::Synced && s.last <= l {
            // дубликат уже применённого сообщения
            return ApplyOutcome::Stale;
        }
        let got = s.prev.unwrap_or(s.first);
        self.invalidate();
        ApplyOutcome::Gap { expected: l, got }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::Decimal;
    use sh_core::{Px, Qty, Side};

    fn lvl(px: i64, qty: i64) -> Level {
        Level::new(Px(Decimal::from(px)), Qty(Decimal::from(qty)))
    }

    fn lighter_delta(begin: u64, nonce: u64, bid: (i64, i64)) -> BookDelta {
        BookDelta {
            seq: DeltaSeq { first: begin, last: nonce, prev: Some(begin) },
            bids: vec![lvl(bid.0, bid.1)],
            asks: vec![],
        }
    }

    fn aster_delta(u_first: u64, u_last: u64, pu: u64, ask: (i64, i64)) -> BookDelta {
        BookDelta {
            seq: DeltaSeq { first: u_first, last: u_last, prev: Some(pu) },
            bids: vec![],
            asks: vec![lvl(ask.0, ask.1)],
        }
    }

    #[test]
    fn lighter_chain_and_gap() {
        let mut b = SequencedBook::new(16);
        assert_eq!(b.apply_snapshot(100, &[lvl(10, 1)], &[lvl(11, 1)]), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(lighter_delta(100, 105, (10, 2))), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(lighter_delta(105, 107, (9, 1))), ApplyOutcome::Applied);
        assert_eq!(b.book().best_bid(), Some(lvl(10, 2)));
        // пропущено сообщение 107..110
        assert_eq!(b.apply_delta(lighter_delta(110, 112, (9, 5))), ApplyOutcome::Gap { expected: 107, got: 110 });
        assert_eq!(b.state(), SyncState::AwaitingSnapshot);
        assert!(b.book().is_empty());
    }

    #[test]
    fn aster_snapshot_with_buffered_events() {
        let mut b = SequencedBook::new(16);
        // события до снимка буферизуются
        assert_eq!(b.apply_delta(aster_delta(90, 95, 89, (20, 1))), ApplyOutcome::Buffered);
        assert_eq!(b.apply_delta(aster_delta(96, 101, 95, (21, 1))), ApplyOutcome::Buffered);
        assert_eq!(b.apply_delta(aster_delta(102, 104, 101, (22, 1))), ApplyOutcome::Buffered);
        // снимок с lastUpdateId=98: первое событие отброшено как устаревшее,
        // второе покрывает 98 (96 <= 98 <= 101), третье стыкуется по pu
        assert_eq!(b.apply_snapshot(98, &[lvl(10, 1)], &[lvl(30, 1)]), ApplyOutcome::Applied);
        assert_eq!(b.last_seq(), 104);
        assert_eq!(b.book().best_ask(), Some(lvl(21, 1)));
        assert_eq!(b.state(), SyncState::Synced);
        assert_eq!(b.apply_delta(aster_delta(105, 106, 104, (23, 1))), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(aster_delta(110, 111, 108, (23, 1))), ApplyOutcome::Gap { expected: 106, got: 108 });
    }

    #[test]
    fn snapshot_newer_than_buffer_gives_gap() {
        let mut b = SequencedBook::new(16);
        b.apply_delta(aster_delta(50, 60, 49, (1, 1)));
        // снимок 40 старее буфера: событие 50..60 не покрывает 40 — разрыв
        assert!(matches!(b.apply_snapshot(40, &[], &[]), ApplyOutcome::Gap { .. }));
    }

    #[test]
    fn duplicate_is_stale() {
        let mut b = SequencedBook::new(4);
        b.apply_snapshot(10, &[], &[]);
        assert_eq!(b.apply_delta(lighter_delta(10, 12, (1, 1))), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(lighter_delta(10, 12, (1, 1))), ApplyOutcome::Stale);
        assert_eq!(b.apply_delta(lighter_delta(12, 13, (1, 2))), ApplyOutcome::Applied);
    }

    #[test]
    fn contiguous_without_prev() {
        let mut b = SequencedBook::new(4);
        b.apply_snapshot(10, &[], &[]);
        let d = |f, l| BookDelta { seq: DeltaSeq { first: f, last: l, prev: None }, bids: vec![], asks: vec![] };
        assert_eq!(b.apply_delta(d(11, 12)), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(d(13, 13)), ApplyOutcome::Applied);
        assert_eq!(b.apply_delta(d(15, 16)), ApplyOutcome::Gap { expected: 13, got: 15 });
    }

    #[test]
    fn buffer_is_bounded() {
        let mut b = SequencedBook::new(2);
        for i in 0..5u64 {
            b.apply_delta(lighter_delta(i * 2, i * 2 + 2, (1, 1)));
        }
        // остались две последние дельты 6..8 и 8..10; снимок 6 стыкуется
        assert_eq!(b.apply_snapshot(6, &[], &[]), ApplyOutcome::Applied);
        assert_eq!(b.last_seq(), 10);
    }

    /// Генерирует цепочку дельт: (ширина шага, сторона, цена, количество).
    fn arb_chain() -> impl Strategy<Value = Vec<(u64, bool, i64, i64)>> {
        proptest::collection::vec((1u64..5, any::<bool>(), 1i64..40, 0i64..4), 1..80)
    }

    proptest! {
        #[test]
        fn contiguous_chain_applies_and_matches_plain_book(start in 0u64..1_000_000, chain in arb_chain()) {
            let mut seq_book = SequencedBook::new(8);
            let mut plain = OrderBook::new();
            seq_book.apply_snapshot(start, &[], &[]);
            let mut prev = start;
            for (step, buy, px, qty) in &chain {
                let level = lvl(*px, *qty);
                let (bids, asks) = if *buy { (vec![level], vec![]) } else { (vec![], vec![level]) };
                let d = BookDelta { seq: DeltaSeq { first: prev + 1, last: prev + step, prev: Some(prev) }, bids, asks };
                prop_assert_eq!(seq_book.apply_delta(d), ApplyOutcome::Applied);
                plain.set_level(if *buy { Side::Buy } else { Side::Sell }, level);
                prev += step;
            }
            prop_assert_eq!(seq_book.book().top(Side::Buy, usize::MAX), plain.top(Side::Buy, usize::MAX));
            prop_assert_eq!(seq_book.book().top(Side::Sell, usize::MAX), plain.top(Side::Sell, usize::MAX));
        }

        #[test]
        fn dropping_any_message_is_detected(start in 0u64..1_000, chain in arb_chain(), drop_idx in any::<prop::sample::Index>()) {
            prop_assume!(chain.len() >= 2);
            // выбрасываем любое сообщение, кроме последнего (после последнего разрыв не виден)
            let drop = drop_idx.index(chain.len() - 1);
            let mut b = SequencedBook::new(8);
            b.apply_snapshot(start, &[], &[]);
            let mut prev = start;
            let mut detected = false;
            for (i, (step, _, px, qty)) in chain.iter().enumerate() {
                let d = BookDelta { seq: DeltaSeq { first: prev + 1, last: prev + step, prev: Some(prev) }, bids: vec![lvl(*px, *qty)], asks: vec![] };
                prev += step;
                if i == drop { continue; }
                if let ApplyOutcome::Gap { .. } = b.apply_delta(d) { detected = true; break; }
            }
            prop_assert!(detected);
        }
    }
}
