//! Slot map whose slots are stored in fixed-size pages instead of one growing `Vec`.

use std::{fmt, marker::PhantomData, ops::{Index, IndexMut}};

use slotmap::{Key, KeyData};

/// Most bytes of slots per page. Pages this large are mapped apart from the small
/// allocations, so long-lived pages do not pin the space freed around them.
const PAGE_BYTES: usize = 2 * 1024 * 1024;

struct Slot<V> {
    /// Odd while the slot holds a value, as in `slotmap`.
    version: u32,
    value: Option<V>,
}

/// A `slotmap::SlotMap` with the same keys, stored in pages of at most `PAGE_BYTES`.
///
/// `SlotMap` keeps its slots in one `Vec` that doubles: with 664k variables the table holds
/// room for 1M, and every doubling copies the whole table. Here the unused room is at most
/// one page per map, and growing never moves a slot.
///
/// Keys are given out as `SlotMap` gives them (slot 0 unused, the slot freed last is reused
/// first, the version odd while occupied), so a key, the iteration order and the slot an
/// insertion reuses are the ones `SlotMap` would give.
pub struct PagedSlotMap<K: Key, V> {
    pages: Vec<Vec<Slot<V>>>,
    /// Free slots, the one freed last at the end.
    free: Vec<u32>,
    num_elems: u32,
    _k: PhantomData<fn(K) -> K>,
}

impl<K: Key, V> Default for PagedSlotMap<K, V> {
    fn default() -> Self {
        Self::with_key()
    }
}

impl<K: Key, V> PagedSlotMap<K, V> {
    /// log2 of the slots per page: a power of two, so a slot is found by a shift and a mask.
    const PAGE_SHIFT: u32 = {
        let per_page = PAGE_BYTES / std::mem::size_of::<Slot<V>>();
        if per_page <= 1 { 0 } else { per_page.ilog2() }
    };
    const PER_PAGE: usize = 1 << Self::PAGE_SHIFT;

    pub fn with_key() -> Self {
        let mut map = Self { pages: vec![], free: vec![], num_elems: 0, _k: PhantomData };
        // Slot 0 is never given out, like the sentinel of `SlotMap`.
        map.push_slot(Slot { version: 0, value: None });
        map
    }

    pub fn len(&self) -> usize {
        self.num_elems as usize
    }

    pub fn is_empty(&self) -> bool {
        self.num_elems == 0
    }

    /// Slots allocated, used or not.
    pub fn capacity(&self) -> usize {
        self.pages.iter().map(|page| page.capacity()).sum()
    }

    fn slot_count(&self) -> usize {
        match self.pages.last() {
            Some(last) => (self.pages.len() - 1) * Self::PER_PAGE + last.len(),
            None => 0,
        }
    }

    fn push_slot(&mut self, slot: Slot<V>) {
        match self.pages.last_mut() {
            Some(last) if last.len() < Self::PER_PAGE => {
                if last.len() == last.capacity() {
                    // The first page grows by doubling, up to a page, for the small maps.
                    let len = last.len();
                    last.reserve_exact(len.max(4).min(Self::PER_PAGE - len));
                }
                last.push(slot);
            },
            _ => {
                let mut page = Vec::with_capacity(if self.pages.is_empty() { 4.min(Self::PER_PAGE) } else { Self::PER_PAGE });
                page.push(slot);
                self.pages.push(page);
            },
        }
    }

    fn slot(&self, idx: u32) -> Option<&Slot<V>> {
        let idx = idx as usize;
        self.pages.get(idx >> Self::PAGE_SHIFT)?.get(idx & (Self::PER_PAGE - 1))
    }

    fn slot_mut(&mut self, idx: u32) -> Option<&mut Slot<V>> {
        let idx = idx as usize;
        self.pages.get_mut(idx >> Self::PAGE_SHIFT)?.get_mut(idx & (Self::PER_PAGE - 1))
    }

    /// (index, version) of a key.
    fn split(key: K) -> (u32, u32) {
        let ffi = key.data().as_ffi();
        (ffi as u32, (ffi >> 32) as u32)
    }

    fn key_of(idx: u32, version: u32) -> K {
        KeyData::from_ffi((u64::from(version) << 32) | u64::from(idx)).into()
    }

    pub fn insert(&mut self, value: V) -> K {
        if let Some(idx) = self.free.pop() {
            let slot = self.slot_mut(idx).unwrap();
            slot.version |= 1;
            slot.value = Some(value);
            let version = slot.version;
            self.num_elems += 1;
            return Self::key_of(idx, version);
        }
        let idx = self.slot_count();
        assert!(idx < u32::MAX as usize, "PagedSlotMap is full");
        self.push_slot(Slot { version: 1, value: Some(value) });
        self.num_elems += 1;
        Self::key_of(idx as u32, 1)
    }

    pub fn contains_key(&self, key: K) -> bool {
        let (idx, version) = Self::split(key);
        self.slot(idx).is_some_and(|slot| slot.version == version)
    }

    pub fn get(&self, key: K) -> Option<&V> {
        let (idx, version) = Self::split(key);
        self.slot(idx).filter(|slot| slot.version == version).and_then(|slot| slot.value.as_ref())
    }

    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let (idx, version) = Self::split(key);
        self.slot_mut(idx).filter(|slot| slot.version == version).and_then(|slot| slot.value.as_mut())
    }

    pub fn remove(&mut self, key: K) -> Option<V> {
        let (idx, version) = Self::split(key);
        let slot = self.slot_mut(idx).filter(|slot| slot.version == version)?;
        let value = slot.value.take();
        slot.version = slot.version.wrapping_add(1);
        self.free.push(idx);
        self.num_elems -= 1;
        value
    }

    /// The values with their keys, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.pages.iter().flatten().enumerate().filter_map(|(idx, slot)| {
            slot.value.as_ref().map(|value| (Self::key_of(idx as u32, slot.version), value))
        })
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.pages.iter().flatten().filter_map(|slot| slot.value.as_ref())
    }
}

impl<K: Key, V> Index<K> for PagedSlotMap<K, V> {
    type Output = V;

    fn index(&self, key: K) -> &V {
        self.get(key).expect("invalid PagedSlotMap key used")
    }
}

impl<K: Key, V> IndexMut<K> for PagedSlotMap<K, V> {
    fn index_mut(&mut self, key: K) -> &mut V {
        self.get_mut(key).expect("invalid PagedSlotMap key used")
    }
}

impl<K: Key, V: fmt::Debug> fmt::Debug for PagedSlotMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use slotmap::{new_key_type, SlotMap};

    use super::*;

    new_key_type! { struct TestKey; }

    /// The same operations give the same keys, values and order as `SlotMap`.
    #[test]
    fn same_keys_as_slotmap() {
        let mut paged: PagedSlotMap<TestKey, [u64; 2048]> = PagedSlotMap::with_key();
        let mut reference: SlotMap<TestKey, [u64; 2048]> = SlotMap::with_key();
        // Several pages.
        const { assert!(PagedSlotMap::<TestKey, [u64; 2048]>::PER_PAGE * 3 < 500) };
        let mut keys = vec![];
        for i in 0..500u64 {
            let key = paged.insert([i; 2048]);
            assert_eq!(key, reference.insert([i; 2048]));
            keys.push(key);
            if i % 3 == 0 {
                let removed = keys.remove(keys.len() / 2);
                assert_eq!(paged.remove(removed), reference.remove(removed));
                assert_eq!(paged.remove(removed), None);
                assert!(!paged.contains_key(removed));
                assert!(paged.get(removed).is_none());
            }
        }
        assert_eq!(paged.len(), reference.len());
        assert!(paged.iter().map(|(k, v)| (k, v[0])).eq(reference.iter().map(|(k, v)| (k, v[0]))));
        assert!(paged.values().map(|v| v[0]).eq(reference.values().map(|v| v[0])));
        for key in keys {
            assert!(paged.contains_key(key));
            assert_eq!(paged[key], reference[key]);
            paged[key][1] = 7;
            assert_eq!(paged.get(key).unwrap()[1], 7);
        }
        assert!(!paged.contains_key(TestKey::null()));
        assert!(!paged.contains_key(KeyData::from_ffi(1u64 << 32).into()));
    }

    #[test]
    fn small_map_stays_small() {
        let mut map: PagedSlotMap<TestKey, u64> = PagedSlotMap::with_key();
        for i in 0..10 {
            map.insert(i);
        }
        assert!(map.capacity() < 32);
        assert!(map.capacity() <= PagedSlotMap::<TestKey, u64>::PER_PAGE);
    }
}
