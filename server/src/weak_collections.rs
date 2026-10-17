use std::{cell::RefCell, collections::{hash_map}, hash::Hash, vec::IntoIter};

use thin_vec::ThinVec;

use crate::{core::symbols::symbol_keys::KeyValidator, utils::{HashMap, HashSet}};

/// Keys held in a list searched linearly, up to this many; a hash set above.
const SMALL_SET: usize = 16;

/// The keys of a [`WeakSet`].
///
/// Most sets hold a few keys (the dependencies of a file, the declarations of an xml id): a
/// `ThinVec` is a pointer with no heap when empty and 16 bytes of header otherwise, where a
/// `HashSet` takes 32 bytes inline and a table of 4 buckets from the first key.
#[derive(Debug, Clone)]
enum Keys<K: Eq + Hash + Copy> {
    Small(ThinVec<K>),
    Large(Box<HashSet<K>>),
}

impl<K: Eq + Hash + Copy> Keys<K> {
    fn insert(&mut self, key: K) -> bool {
        match self {
            Keys::Small(keys) => {
                if keys.contains(&key) {
                    return false;
                }
                if keys.len() < SMALL_SET {
                    keys.push(key);
                } else {
                    let mut set: HashSet<K> = keys.iter().copied().collect();
                    set.insert(key);
                    *self = Keys::Large(Box::new(set));
                }
                true
            },
            Keys::Large(set) => set.insert(key),
        }
    }

    fn contains(&self, key: &K) -> bool {
        match self {
            Keys::Small(keys) => keys.contains(key),
            Keys::Large(set) => set.contains(key),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Keys::Small(keys) => keys.is_empty(),
            Keys::Large(set) => set.is_empty(),
        }
    }

    fn remove(&mut self, key: &K) {
        match self {
            Keys::Small(keys) => keys.retain(|k| k != key),
            Keys::Large(set) => { set.remove(key); },
        }
    }

    fn retain(&mut self, mut f: impl FnMut(&K) -> bool) {
        match self {
            Keys::Small(keys) => keys.retain(|k| f(k)),
            Keys::Large(set) => set.retain(|k| f(k)),
        }
    }

    fn to_vec(&self) -> Vec<K> {
        match self {
            Keys::Small(keys) => keys.to_vec(),
            Keys::Large(set) => set.iter().copied().collect(),
        }
    }

    fn take(&mut self) -> Vec<K> {
        let keys = self.to_vec();
        *self = Keys::Small(ThinVec::new());
        keys
    }
}

#[derive(Debug, Clone)]
pub struct WeakSet<K: Eq + Hash + Copy> {
    set: RefCell<Keys<K>>,
}

impl<K: Eq + Hash + Copy> Default for WeakSet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy> WeakSet<K> {
    pub fn new() -> Self {
        Self {
            set: RefCell::new(Keys::Small(ThinVec::new())),
        }
    }

    pub fn insert(&mut self, key: K) -> bool {
        self.set.get_mut().insert(key)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.set.borrow().contains(key)
    }

    pub fn is_empty(&self, table: &impl KeyValidator<K>) -> bool {
        self.set.borrow_mut().retain(|k| table.is_key_valid(*k));
        self.set.borrow().is_empty()
    }

    pub fn remove(&mut self, key: &K) {
        self.set.get_mut().remove(key);
    }

    pub fn clear_invalid(&mut self, table: &impl KeyValidator<K>) {
        self.set.get_mut().retain(|k| table.is_key_valid(*k));
    }

    pub fn iter_valid(&self, table: &impl KeyValidator<K>) -> IntoIter<K> {
        let mut set = self.set.borrow_mut();
        set.retain(|k| table.is_key_valid(*k));
        set.to_vec().into_iter()
    }

    pub fn retain_valid(&mut self, table: &impl KeyValidator<K>, f: impl Fn(&K) -> bool) {
        self.set.get_mut().retain(|k| table.is_key_valid(*k) && f(k));
    }

    pub fn drain_valid(&mut self, table: &impl KeyValidator<K>) -> IntoIter<K> {
        let mut valid = self.set.get_mut().take();
        valid.retain(|k| table.is_key_valid(*k));
        valid.into_iter()
    }
}

/// Map with weak keys
#[derive(Debug, Clone)]
pub struct WeakMap<K: Eq + Hash + Copy, V> {
    map: HashMap<K, V>,
}

impl<K: Eq + Hash + Copy, V> Default for WeakMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy, V> WeakMap<K, V> {
    pub fn new() -> Self {
        Self {
            map: HashMap::default(),
        }
    }

    pub fn entry(&mut self, key: K) -> hash_map::Entry<'_, K, V> {
        self.map.entry(key)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.remove(key)
    }

    /// iterate over values of valid keys
    pub fn iter_valid_values(&mut self, table: &impl KeyValidator<K>) -> impl Iterator<Item = &V> {
        self.map.retain(|k, _| table.is_key_valid(*k));
        self.map.values()
    }
}
