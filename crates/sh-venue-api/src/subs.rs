//! План подписок: распределяет потоки по соединениям с учётом лимитов площадки.
//! Желаемый список упорядочен по приоритету; всё, что не помещается в общий
//! лимит, попадает в «непокрытые» и сообщается в логах и метриках.

use std::collections::{BTreeMap, BTreeSet};

/// Изменения плана для одного пересчёта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiff<T> {
    /// Добавить: (номер соединения, потоки).
    pub adds: Vec<(usize, Vec<T>)>,
    /// Удалить: (номер соединения, потоки).
    pub removes: Vec<(usize, Vec<T>)>,
    /// Не поместились в лимиты.
    pub uncovered: Vec<T>,
}

impl<T> PlanDiff<T> {
    /// Истина, если менять нечего.
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty() && self.removes.is_empty()
    }
}

/// Распределение подписок по фиксированному числу соединений.
#[derive(Debug, Clone)]
pub struct SubPlan<T: Ord + Clone> {
    conns: Vec<BTreeSet<T>>,
    per_conn: usize,
    max_total: usize,
}

impl<T: Ord + Clone> SubPlan<T> {
    /// `conns` соединений по `per_conn` подписок, но не более `max_total` всего.
    pub fn new(conns: usize, per_conn: usize, max_total: usize) -> Self {
        Self { conns: vec![BTreeSet::new(); conns.max(1)], per_conn: per_conn.max(1), max_total }
    }

    /// Число соединений.
    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }

    /// Текущие подписки соединения.
    pub fn conn_subs(&self, conn: usize) -> Option<&BTreeSet<T>> {
        self.conns.get(conn)
    }

    /// Всего подписок.
    pub fn total(&self) -> usize {
        self.conns.iter().map(BTreeSet::len).sum()
    }

    /// Пересчитывает план под новый желаемый список (по убыванию приоритета).
    /// Уже назначенные потоки остаются на своих соединениях.
    pub fn apply(&mut self, desired: &[T]) -> PlanDiff<T> {
        let capacity = self.max_total.min(self.per_conn * self.conns.len());
        let mut seen = BTreeSet::new();
        let unique: Vec<&T> = desired.iter().filter(|t| seen.insert((*t).clone())).collect();
        let keep: BTreeSet<T> = unique.iter().take(capacity).map(|t| (*t).clone()).collect();
        let uncovered: Vec<T> = unique.iter().skip(capacity).map(|t| (*t).clone()).collect();

        let mut removes: BTreeMap<usize, Vec<T>> = BTreeMap::new();
        for (i, set) in self.conns.iter_mut().enumerate() {
            let gone: Vec<T> = set.iter().filter(|t| !keep.contains(*t)).cloned().collect();
            for t in &gone {
                set.remove(t);
            }
            if !gone.is_empty() {
                removes.insert(i, gone);
            }
        }

        let assigned: BTreeSet<T> = self.conns.iter().flatten().cloned().collect();
        let mut adds: BTreeMap<usize, Vec<T>> = BTreeMap::new();
        let mut overflow = Vec::new();
        for t in unique.into_iter().take(capacity) {
            if assigned.contains(t) {
                continue;
            }
            // наименее загруженное соединение со свободным местом
            let target = self
                .conns
                .iter()
                .enumerate()
                .filter(|(_, s)| s.len() < self.per_conn)
                .min_by_key(|(_, s)| s.len())
                .map(|(i, _)| i);
            match target {
                Some(i) => {
                    self.conns[i].insert(t.clone());
                    adds.entry(i).or_default().push(t.clone());
                }
                None => overflow.push(t.clone()),
            }
        }
        let mut uncovered = uncovered;
        uncovered.extend(overflow);
        PlanDiff { adds: adds.into_iter().collect(), removes: removes.into_iter().collect(), uncovered }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn respects_caps_and_priority() {
        let mut p = SubPlan::new(2, 3, 5);
        let d = p.apply(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(p.total(), 5);
        assert_eq!(d.uncovered, vec![6, 7]);
        assert!(d.removes.is_empty());
        // нагрузка распределена равномерно
        assert!(p.conn_subs(0).unwrap().len() <= 3 && p.conn_subs(1).unwrap().len() <= 3);
    }

    #[test]
    fn stable_assignment_and_minimal_diff() {
        let mut p = SubPlan::new(2, 10, 100);
        p.apply(&[1, 2, 3, 4]);
        let before0 = p.conn_subs(0).unwrap().clone();
        let d = p.apply(&[1, 2, 3, 4, 5]);
        assert_eq!(d.adds.iter().map(|(_, v)| v.len()).sum::<usize>(), 1);
        assert!(d.removes.is_empty());
        assert!(before0.is_subset(p.conn_subs(0).unwrap()) || before0.is_subset(p.conn_subs(1).unwrap()));
        let d = p.apply(&[1, 3, 5]);
        assert_eq!(d.removes.iter().flat_map(|(_, v)| v.clone()).collect::<BTreeSet<_>>(), BTreeSet::from([2, 4]));
        assert!(d.adds.is_empty());
    }

    #[test]
    fn higher_priority_displaces_lower() {
        let mut p = SubPlan::new(1, 3, 3);
        p.apply(&[10, 20, 30]);
        let d = p.apply(&[5, 10, 20, 30]);
        assert_eq!(d.removes, vec![(0, vec![30])]);
        assert_eq!(d.adds, vec![(0, vec![5])]);
        assert_eq!(d.uncovered, vec![30]);
    }

    proptest! {
        #[test]
        fn plan_invariants(rounds in proptest::collection::vec(proptest::collection::vec(0u16..300, 0..300), 1..5), conns in 1usize..6, per in 1usize..80, max in 0usize..400) {
            let mut p = SubPlan::new(conns, per, max);
            for desired in rounds {
                let d = p.apply(&desired);
                let cap = max.min(per * conns);
                let mut uniq = Vec::new();
                for x in &desired { if !uniq.contains(x) { uniq.push(*x); } }
                // назначены ровно первые `cap` уникальных по приоритету
                let want: BTreeSet<u16> = uniq.iter().take(cap).copied().collect();
                let got: BTreeSet<u16> = (0..p.conn_count()).flat_map(|i| p.conn_subs(i).unwrap().iter().copied().collect::<Vec<_>>()).collect();
                prop_assert_eq!(&got, &want);
                prop_assert_eq!(p.total(), want.len());
                prop_assert!((0..p.conn_count()).all(|i| p.conn_subs(i).unwrap().len() <= per));
                prop_assert_eq!(d.uncovered.len(), uniq.len() - want.len());
            }
        }
    }
}
