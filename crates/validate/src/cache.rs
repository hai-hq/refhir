//! A bounded cache for values that are read from the store when first used.
//!
//! The catalog keeps an index of every definition, code system and value set expansion and
//! reads the one a request needs when it needs it. What was read is kept here, up to a budget,
//! so the things requests use over and over (the definitions of the common resource types, the
//! value sets they bind) are read once, and the rest does not stay in memory.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, PoisonError, RwLock};

struct Slot<V> {
    value: Arc<V>,
    weight: usize,
    /// When the slot was last read, as a tick of the cache's clock. Read under the shared lock,
    /// so a hit does not wait for another hit.
    used: AtomicU64,
}

struct Slots<K, V> {
    by_key: HashMap<K, Slot<V>>,
    weight: usize,
}

/// Values by key, weighed in bytes, with the least recently used dropped when the weight passes
/// the budget. The value just added is always kept, even when it alone is over the budget: a
/// code system larger than the budget is read once, not on every request.
pub(crate) struct Cache<K, V> {
    slots: RwLock<Slots<K, V>>,
    clock: AtomicU64,
    budget: usize,
}

impl<K: Hash + Eq + Clone, V> Cache<K, V> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            slots: RwLock::new(Slots {
                by_key: HashMap::new(),
                weight: 0,
            }),
            clock: AtomicU64::new(0),
            budget,
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Relaxed)
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let slots = self.slots.read().unwrap_or_else(PoisonError::into_inner);
        let slot = slots.by_key.get(key)?;
        slot.used.store(self.tick(), Relaxed);
        Some(slot.value.clone())
    }

    /// The value under `key`, read by `load` when it is not held. `load` runs without any lock,
    /// so two requests that miss together may both read it; the first to finish is kept, and the
    /// two are equal. `load` gives the value and its weight, or `None` when there is none, which
    /// is not remembered. `own` makes the key to hold it under, so a lookup that hits does not
    /// build one.
    pub(crate) fn get_or_load<Q>(
        &self,
        key: &Q,
        own: impl FnOnce(&Q) -> K,
        load: impl FnOnce() -> Option<(V, usize)>,
    ) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(held) = self.get(key) {
            return Some(held);
        }
        let (value, weight) = load()?;
        Some(self.insert(own(key), Arc::new(value), weight))
    }

    /// Hold `value`, or return the one another caller put under `key` first.
    pub(crate) fn insert(&self, key: K, value: Arc<V>, weight: usize) -> Arc<V> {
        let mut slots = self.slots.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = slots.by_key.get(&key) {
            existing.used.store(self.tick(), Relaxed);
            return existing.value.clone();
        }
        slots.weight += weight;
        slots.by_key.insert(
            key.clone(),
            Slot {
                value: value.clone(),
                weight,
                used: AtomicU64::new(self.tick()),
            },
        );
        if slots.weight > self.budget {
            let mut by_age: Vec<(u64, K)> = slots
                .by_key
                .iter()
                .filter(|(held, _)| **held != key)
                .map(|(held, slot)| (slot.used.load(Relaxed), held.clone()))
                .collect();
            by_age.sort_unstable_by_key(|(used, _)| *used);
            for (_, evicted) in by_age {
                if slots.weight <= self.budget {
                    break;
                }
                if let Some(slot) = slots.by_key.remove(&evicted) {
                    slots.weight -= slot.weight;
                }
            }
        }
        value
    }

    /// Forget every value whose key `stale` picks, because what it was read from changed.
    pub(crate) fn forget(&self, stale: impl Fn(&K) -> bool) {
        let mut slots = self.slots.write().unwrap_or_else(PoisonError::into_inner);
        let mut gone = 0;
        slots.by_key.retain(|key, slot| {
            let keep = !stale(key);
            if !keep {
                gone += slot.weight;
            }
            keep
        });
        slots.weight -= gone;
    }

    /// Bytes of the values held, by their weights.
    #[cfg(test)]
    pub(crate) fn weight(&self) -> usize {
        self.slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .weight
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .by_key
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(cache: &Cache<u32, String>, key: u32, weight: usize) -> Arc<String> {
        cache
            .get_or_load(&key, |key| *key, || Some((format!("v{key}"), weight)))
            .unwrap()
    }

    #[test]
    fn a_value_is_read_once_and_then_held() {
        let cache = Cache::new(100);
        let reads = std::cell::Cell::new(0);
        for _ in 0..3 {
            let value = cache
                .get_or_load(
                    &1,
                    |key| *key,
                    || {
                        reads.set(reads.get() + 1);
                        Some(("one".to_string(), 10))
                    },
                )
                .unwrap();
            assert_eq!(*value, "one");
        }
        assert_eq!(reads.get(), 1);
        assert_eq!(cache.weight(), 10);
    }

    #[test]
    fn what_is_missing_is_not_remembered() {
        let cache: Cache<u32, String> = Cache::new(100);
        assert!(cache.get_or_load(&1, |key| *key, || None).is_none());
        assert_eq!(
            *cache
                .get_or_load(&1, |key| *key, || Some(("now".into(), 1)))
                .unwrap(),
            "now"
        );
    }

    #[test]
    fn the_least_recently_used_goes_first_and_the_weight_stays_in_budget() {
        let cache = Cache::new(30);
        load(&cache, 1, 10);
        load(&cache, 2, 10);
        load(&cache, 3, 10);
        // Reading 1 makes 2 the oldest.
        assert!(cache.get(&1).is_some());
        load(&cache, 4, 10);
        assert!(cache.get(&2).is_none());
        assert!(cache.get(&1).is_some() && cache.get(&3).is_some() && cache.get(&4).is_some());
        assert_eq!(cache.weight(), 30);
    }

    #[test]
    fn a_value_over_the_budget_is_kept_alone() {
        let cache = Cache::new(30);
        load(&cache, 1, 10);
        load(&cache, 2, 100);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&2).is_some());
        assert_eq!(cache.len(), 1);
        // It goes when something else comes in, and the weight is what is held.
        load(&cache, 3, 10);
        assert!(cache.get(&2).is_none());
        assert_eq!(cache.weight(), 10);
    }

    #[test]
    fn what_changed_is_forgotten() {
        let cache = Cache::new(100);
        for key in 1..=4 {
            load(&cache, key, 10);
        }
        cache.forget(|key| key % 2 == 0);
        assert!(cache.get(&2).is_none() && cache.get(&4).is_none());
        assert!(cache.get(&1).is_some() && cache.get(&3).is_some());
        assert_eq!(cache.weight(), 20);
    }

    #[test]
    fn two_readers_that_miss_together_share_the_first_value() {
        let cache = Cache::new(100);
        let first = cache.insert(1, Arc::new("first".to_string()), 5);
        let second = cache.insert(1, Arc::new("second".to_string()), 5);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.weight(), 5);
    }
}
