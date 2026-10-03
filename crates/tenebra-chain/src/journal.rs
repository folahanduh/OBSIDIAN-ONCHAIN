//! Transaction-scoped undo journal.
//!
//! A transaction either applies completely or not at all (apart from its gas
//! fee). Every state map records the previous value of each key it touches
//! while a transaction is open; `revert` restores them in reverse order. This
//! keeps handlers simple — they may fail half-way — without cloning state.

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Journaled<K: Ord + Clone, V: Clone> {
    map: BTreeMap<K, V>,
    journal: Vec<(K, Option<V>)>,
    active: bool,
}

impl<K: Ord + Clone, V: Clone> Default for Journaled<K, V> {
    fn default() -> Self {
        Journaled {
            map: BTreeMap::new(),
            journal: Vec::new(),
            active: false,
        }
    }
}

impl<K: Ord + Clone, V: Clone> Journaled<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> {
        self.map.get(k)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    pub fn insert(&mut self, k: K, v: V) {
        let old = self.map.insert(k.clone(), v);
        if self.active {
            self.journal.push((k, old));
        }
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let old = self.map.remove(k);
        if self.active && old.is_some() {
            self.journal.push((k.clone(), old.clone()));
        }
        old
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> {
        self.map.iter()
    }

    pub fn range<R: std::ops::RangeBounds<K>>(
        &self,
        r: R,
    ) -> impl DoubleEndedIterator<Item = (&K, &V)> {
        self.map.range(r)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn begin(&mut self) {
        debug_assert!(self.journal.is_empty());
        self.active = true;
    }

    pub fn commit(&mut self) {
        self.journal.clear();
        self.active = false;
    }

    pub fn revert(&mut self) {
        while let Some((k, old)) = self.journal.pop() {
            match old {
                Some(v) => {
                    self.map.insert(k, v);
                }
                None => {
                    self.map.remove(&k);
                }
            }
        }
        self.active = false;
    }
}

/// Journaled single value.
#[derive(Clone, Debug, Default)]
pub struct JournaledCell<T: Clone> {
    value: T,
    saved: Option<T>,
    active: bool,
}

impl<T: Clone> JournaledCell<T> {
    pub fn new(value: T) -> Self {
        JournaledCell {
            value,
            saved: None,
            active: false,
        }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn set(&mut self, v: T) {
        if self.active && self.saved.is_none() {
            self.saved = Some(self.value.clone());
        }
        self.value = v;
    }

    pub fn begin(&mut self) {
        self.active = true;
    }

    pub fn commit(&mut self) {
        self.saved = None;
        self.active = false;
    }

    pub fn revert(&mut self) {
        if let Some(s) = self.saved.take() {
            self.value = s;
        }
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_restores_inserts_updates_and_removes() {
        let mut m: Journaled<u32, u32> = Journaled::default();
        m.insert(1, 10);
        m.insert(2, 20);
        m.begin();
        m.insert(1, 11);
        m.insert(1, 12);
        m.insert(3, 30);
        m.remove(&2);
        m.revert();
        assert_eq!(
            m.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
            vec![(1, 10), (2, 20)]
        );
        m.begin();
        m.insert(1, 99);
        m.commit();
        assert_eq!(m.get(&1), Some(&99));
    }

    #[test]
    fn cell_reverts_to_first_saved_value() {
        let mut c = JournaledCell::new(5u64);
        c.begin();
        c.set(6);
        c.set(7);
        c.revert();
        assert_eq!(*c.get(), 5);
        c.set(8); // outside a tx: applied directly
        assert_eq!(*c.get(), 8);
    }
}
