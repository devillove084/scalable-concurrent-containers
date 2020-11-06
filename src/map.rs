extern crate crossbeam;

pub mod array;
pub mod cell;

use array::Array;
use cell::EntryLink;
use cell::{Cell, ExclusiveLocker, SharedLocker};
use crossbeam::epoch::{Atomic, Guard, Owned};
use std::convert::TryInto;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// A scalable concurrent hash map implementation.
///
/// The key features of scc::HashMap are as follows.
/// * No sharding: all keys are managed by a single array of key metadata cells.
/// * Auto resizing: it automatically doubles or halves the capacity.
/// * Non-blocking resizing: resizing does not block other threads.
/// * Incremental resizing: access to the data structure relocates a certain number of key-value pairs.
/// * Optimized resizing: a single key-value pair is guaranteed to be relocated to one of the two adjacent cells.
pub struct HashMap<K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> {
    array: Atomic<Array<K, V, Cell<K, V>>>,
    minimum_capacity: usize,
    resize_mutex: AtomicBool,
    hasher: H,
}

impl<K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> HashMap<K, V, H> {
    /// Creates an empty HashMap instance with the given hasher and minimum capacity.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), Some(1000));
    /// ```
    pub fn new(hasher: H, minimum_capacity: Option<usize>) -> HashMap<K, V, H> {
        let initial_capacity = if let Some(capacity) = minimum_capacity {
            capacity
        } else {
            160
        };
        HashMap {
            array: Atomic::new(Array::<K, V, Cell<K, V>>::new(
                initial_capacity,
                Atomic::null(),
            )),
            minimum_capacity: initial_capacity,
            resize_mutex: AtomicBool::new(false),
            hasher: hasher,
        }
    }

    /// Inserts a key-value pair into the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.insert(1, 1);
    /// if let Err((result, value)) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     assert_eq!(value, 1);
    /// }
    /// ```
    pub fn insert<'a>(
        &'a self,
        key: K,
        value: V,
    ) -> Result<Accessor<'a, K, V, H>, (Accessor<'a, K, V, H>, V)> {
        let (hash, partial_hash) = self.hash(&key);
        let (mut accessor, array_ptr, cell_index) = self.acquire(&key, hash, partial_hash);
        if !accessor.key_value_pair_ptr.is_null() {
            return Err((accessor, value));
        }
        match accessor.cell_locker.insert(partial_hash) {
            Some(sub_index) => {
                let key_value_array_index = cell_index * 10 + (sub_index as usize);
                let key_value_pair_ptr =
                    unsafe { (*array_ptr).get_key_value_pair(key_value_array_index) };
                let key_value_pair_mut_ptr = key_value_pair_ptr as *mut (K, V);
                unsafe { key_value_pair_mut_ptr.write((key.clone(), value)) };
                accessor.sub_index = sub_index;
                accessor.key_value_pair_ptr = key_value_pair_ptr;
            }
            None => {
                accessor.key_value_pair_ptr = accessor.cell_locker.insert_link(&key, value);
            }
        };
        Ok(accessor)
    }

    /// Upserts a key-value pair into the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.upsert(1, 1);
    /// assert_eq!(result.get(), (&1, &mut 1));
    /// ```
    pub fn upsert<'a>(&'a self, key: K, value: V) -> Accessor<'a, K, V, H> {
        match self.insert(key, value) {
            Ok(result) => result,
            Err((result, value)) => {
                let pair_mut_ptr = result.key_value_pair_ptr as *mut (K, V);
                unsafe { (*pair_mut_ptr).1 = value };
                result
            }
        }
    }

    /// Gets a reference to the value associated with the key.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert_eq!(result.unwrap().get(), (&1, &mut 0));
    /// ```
    pub fn get<'a>(&'a self, key: K) -> Option<Accessor<'a, K, V, H>> {
        let (hash, partial_hash) = self.hash(&key);
        let (mut accessor, _, _) = self.acquire(&key, hash, partial_hash);
        if accessor.key_value_pair_ptr.is_null() {
            return None;
        }
        Some(accessor)
    }

    /// Removes a key-value pair.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.remove(1);
    /// assert_eq!(result, false);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.remove(1);
    /// assert!(result);
    /// ```
    pub fn remove(&self, key: K) -> bool {
        self.get(key).map_or(false, |accessor| accessor.erase())
    }

    /// Reads the key-value pair.
    pub fn read<U, F: FnOnce(&K, &V) -> U>(&self, key: K, f: F) -> Option<U> {
        let _ = crossbeam::epoch::pin();
        None
    }

    /// Mutates the value associated with the given key.
    pub fn mutate<U, F: FnOnce(&K, &mut V) -> U>(&self, key: K, f: F) -> Option<U> {
        let _ = crossbeam::epoch::pin();
        None
    }

    /// Retains the key-value pairs that the given function allows them to.
    pub fn retain<F: Fn(&K, &V) -> bool>(&self, f: F) -> usize {
        let _ = crossbeam::epoch::pin();
        0
    }

    /// Clear all the key-value pairs stored at the moment.
    pub fn clear(&self) -> usize {
        self.retain(|_, _| false)
    }

    /// Returns the estimated size of the HashMap.
    pub fn len(&self) -> usize {
        let _ = crossbeam::epoch::pin();
        0
    }

    /// Returns a Scanner.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let mut iter = hashmap.iter();
    /// assert_eq!(iter.next(), Some((&1, &mut 0)));
    /// assert_eq!(iter.next(), None);
    ///
    /// for iter in hashmap.iter() {
    ///     assert_eq!(iter, (&1, &mut 0));
    /// }
    /// ```
    pub fn iter<'a>(&'a self) -> Scanner<'a, K, V, H> {
        let guard = crossbeam::epoch::pin();

        // an Acquire fence is required to correctly load the contents of the array
        let current_array_ptr = self.array.load(Acquire, &guard).as_raw();
        let old_array_ptr = unsafe { (*current_array_ptr).get_old_array(&guard) }.as_raw();
        for array_ptr in vec![old_array_ptr, current_array_ptr] {
            if array_ptr.is_null() {
                continue;
            }
            let num_cells = unsafe { (*array_ptr).num_cells() };
            for cell_index in 0..num_cells {
                let cell = unsafe { (*array_ptr).get_cell(cell_index) };
                let locker = ExclusiveLocker::lock(cell);
                if !locker.killed() && !locker.empty() {
                    if locker.overflowing() {
                        let link = locker.link_head();
                        let key_value_pair_ptr = unsafe { (*link).key_value_pair_ptr() };
                        return Scanner {
                            accessor: Some(Accessor {
                                hash_map: &self,
                                cell_locker: locker,
                                sub_index: u8::MAX,
                                key_value_pair_ptr: key_value_pair_ptr,
                            }),
                            array_ptr: array_ptr,
                            cell_index: cell_index,
                            entry_link: link,
                            activated: false,
                        };
                    }
                    for sub_index in 0..10 as u8 {
                        if locker.occupied(sub_index as usize) {
                            let key_value_array_index = cell_index * 10 + (sub_index as usize);
                            let key_value_pair_ptr =
                                unsafe { (*array_ptr).get_key_value_pair(key_value_array_index) };
                            return Scanner {
                                accessor: Some(Accessor {
                                    hash_map: &self,
                                    cell_locker: locker,
                                    sub_index: sub_index,
                                    key_value_pair_ptr: key_value_pair_ptr,
                                }),
                                array_ptr: array_ptr,
                                cell_index: cell_index,
                                entry_link: std::ptr::null(),
                                activated: false,
                            };
                        }
                    }
                }
            }
        }

        Scanner {
            accessor: None,
            array_ptr: std::ptr::null(),
            cell_index: 0,
            entry_link: std::ptr::null(),
            activated: false,
        }
    }

    /// Returns a hash value of the given key.
    fn hash(&self, key: &K) -> (u64, u32) {
        // generate a hash value
        let mut h = self.hasher.build_hasher();
        key.hash(&mut h);
        let mut hash = h.finish();

        // bitmix: https://mostlymangling.blogspot.com/2019/01/better-stronger-mixer-and-test-procedure.html
        hash = hash ^ (hash.rotate_right(25) ^ hash.rotate_right(50));
        hash = hash.overflowing_mul(0xA24BAED4963EE407u64).0;
        hash = hash ^ (hash.rotate_right(24) ^ hash.rotate_right(49));
        hash = hash.overflowing_mul(0x9FB21C651E98DF25u64).0;
        hash = hash ^ (hash >> 28);
        (hash, (hash & ((1 << 32) - 1)).try_into().unwrap())
    }

    /// Acquires a cell.
    fn acquire<'a>(
        &'a self,
        key: &K,
        hash: u64,
        partial_hash: u32,
    ) -> (Accessor<'a, K, V, H>, *const Array<K, V, Cell<K, V>>, usize) {
        let guard = crossbeam::epoch::pin();

        // it is guaranteed that the thread reads a consistent snapshot of current and
        // old array pair by a Release fence at the resize function, hence the following
        // procedure is correct.
        //  - the thread reads self.array, and it kills the target cell in the old array
        //    if there is one attached to it, and inserts the key into array.
        // There are two cases.
        //  1. the thread reads an old version of self.array.
        //    if there is another thread having read the latest version of self.array,
        //    trying to insert the same key, it will try to kill the cell in the old version
        //    of self.array, thus competing with each other.
        //  2. Thread X reads the latest version of self.array.
        //    if the array is deprecated while inserting the key, it falls into case 1.
        loop {
            // an Acquire fence is required to correctly load the contents of the array
            let current_array_ptr = self.array.load(Acquire, &guard).as_raw();
            let old_array_ptr = unsafe { (*current_array_ptr).get_old_array(&guard) }.as_raw();
            if !old_array_ptr.is_null() {
                // relocate at most 16 cells
                // self.relocate(current_array_ptr, old_array_ptr);
                let (locker, key_value_pair_ptr, cell_index, sub_index) =
                    self.search(&key, hash, partial_hash, old_array_ptr);
                if !key_value_pair_ptr.is_null() {
                    return (
                        Accessor {
                            hash_map: &self,
                            cell_locker: locker,
                            sub_index: sub_index,
                            key_value_pair_ptr: key_value_pair_ptr,
                        },
                        current_array_ptr,
                        cell_index,
                    );
                } else if !locker.killed() {
                    // relocated the cell
                    // self.kill(locker, old_array_ptr, current_array_ptr);
                }
            }
            let (locker, key_value_pair_ptr, cell_index, sub_index) =
                self.search(&key, hash, partial_hash, current_array_ptr);
            if !locker.killed() {
                return (
                    Accessor {
                        hash_map: &self,
                        cell_locker: locker,
                        sub_index: sub_index,
                        key_value_pair_ptr: key_value_pair_ptr,
                    },
                    current_array_ptr,
                    cell_index,
                );
            }
            // reaching here indicates that self.array is updated
        }
    }

    /// Erases a key-value pair owned by the accessor.
    fn erase<'a>(&'a self, mut accessor: Accessor<'a, K, V, H>) {
        if accessor.sub_index != u8::MAX {
            accessor.cell_locker.remove(accessor.sub_index);
            let key_value_pair_mut_ptr = accessor.key_value_pair_ptr as *mut (K, V);
            unsafe {
                std::ptr::drop_in_place(key_value_pair_mut_ptr);
            }
        } else {
            accessor
                .cell_locker
                .remove_link(unsafe { &(*accessor.key_value_pair_ptr).0 })
        }
    }

    /// Searches for a cell for the key.
    fn search<'a>(
        &self,
        key: &K,
        hash: u64,
        partial_hash: u32,
        array_ptr: *const Array<K, V, Cell<K, V>>,
    ) -> (ExclusiveLocker<'a, K, V>, *const (K, V), usize, u8) {
        let cell_index = unsafe { (*array_ptr).calculate_metadata_array_index(hash) };
        let cell = unsafe { (*array_ptr).get_cell(cell_index) };
        let locker = ExclusiveLocker::lock(cell);
        if !locker.killed() && !locker.empty() {
            if locker.overflowing() {
                let key_value_pair_ptr = locker.search_link(key);
                if !key_value_pair_ptr.is_null() {
                    return (locker, key_value_pair_ptr, cell_index, u8::MAX);
                }
            }
            if let Some(sub_index) = locker.search(partial_hash) {
                let key_value_array_index = cell_index * 10 + (sub_index as usize);
                let key_value_pair_ptr =
                    unsafe { (*array_ptr).get_key_value_pair(key_value_array_index) };
                if unsafe { (*key_value_pair_ptr).0 == *key } {
                    return (locker, key_value_pair_ptr, cell_index, sub_index);
                }
            }
        }
        (locker, std::ptr::null(), cell_index, 0)
    }
}

impl<K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> Drop for HashMap<K, V, H> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Accessor offer a means of reading a key-value stored in a hash map container.
///
/// It is !Send, thus disallowing other threads to have references to it.
pub struct Accessor<'a, K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> {
    hash_map: &'a HashMap<K, V, H>,
    cell_locker: ExclusiveLocker<'a, K, V>,
    sub_index: u8,
    key_value_pair_ptr: *const (K, V),
}

impl<'a, K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> Accessor<'a, K, V, H> {
    /// Returns a reference to the key-value pair.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     (*result.get().1) = 2;
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert_eq!(result.unwrap().get(), (&1, &mut 2));
    /// ```
    pub fn get(&'a self) -> (&'a K, &'a mut V) {
        unsafe {
            let key_ptr = &(*self.key_value_pair_ptr).0 as *const K;
            let value_ptr = &(*self.key_value_pair_ptr).1 as *const V;
            let value_mut_ptr = value_ptr as *mut V;
            (&(*key_ptr), &mut (*value_mut_ptr))
        }
    }

    /// Erases the key-value pair owned by the Accessor.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     result.erase();
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    /// ```
    pub fn erase(self) -> bool {
        if self.key_value_pair_ptr.is_null() {
            return false;
        }
        self.hash_map.erase(self);
        true
    }
}

/// Scanner implements Iterator.
///
/// It is !Send, thus disallowing other threads to have references to it.
pub struct Scanner<'a, K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> {
    accessor: Option<Accessor<'a, K, V, H>>,
    array_ptr: *const Array<K, V, Cell<K, V>>,
    cell_index: usize,
    entry_link: *const EntryLink<K, V>,
    activated: bool,
}

impl<'a, K: Clone + Eq + Hash + Sync, V: Sync + Unpin, H: BuildHasher> Iterator
    for Scanner<'a, K, V, H>
{
    type Item = (&'a K, &'a mut V);
    fn next(&mut self) -> Option<Self::Item> {
        if !self.activated {
            self.activated = true;
            if let Some(accessor) = &self.accessor {
                unsafe {
                    let key_ptr = &(*accessor.key_value_pair_ptr).0 as *const K;
                    let value_ptr = &(*accessor.key_value_pair_ptr).1 as *const V;
                    let value_mut_ptr = value_ptr as *mut V;
                    return Some((&(*key_ptr), &mut (*value_mut_ptr)))
                }
            }
        }
        self.accessor.take();
        None
    }
}
