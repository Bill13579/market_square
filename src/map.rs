use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::storage::{FixedStorage, FixedStorageMultiple};

#[cfg(any(feature = "std", feature = "alloc"))]
use crate::storage::BoxedSliceStorage;

pub const EMPTY: u64 = 0;
pub const IN_PROGRESS: u64 = 1 << 63;

/// Use for `placement_offset`.
pub const ZERO_OFFSET: usize = 0;

/// Use for `n`. For just grabbing whatever is at the key's index without further probing/searching.
pub const IMMEDIATE: usize = 1;

/// Use for `n`. For searching the entire table.
pub const MAX: usize = usize::MAX;

/// Extract the logical key bits of a key.
#[inline]
pub fn key_bits(k: u64) -> u64 {
    k & !IN_PROGRESS
}

/// Check if the IN_PROGRESS bit is set (also returns false if key is EMPTY).
#[inline]
pub fn is_in_progress(k: u64) -> bool {
    (k & IN_PROGRESS) != 0 && k != EMPTY
}

pub struct SimpleLPHashMap<V, S>
where 
    S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>
{
    mask: usize,
    slots: S,
    phantom: core::marker::PhantomData<V>,
}

pub struct Slot<V> {
    pub key: AtomicU64,                  // 0 = empty, else key (+optional IN_PROGRESS bit)
    pub value: UnsafeCell<MaybeUninit<V>>,
}

impl<V> Slot<V> {
    const fn new() -> Self {
        Self {
            key: AtomicU64::new(EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    #[inline]
    fn load_key(&self) -> u64 {
        self.key.load(Ordering::Acquire)
    }
}

// We allow cross-thread use, but correctness when using the unsafe API is on the caller.
unsafe impl<V: Send + Sync, S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>> Sync for SimpleLPHashMap<V, S> {}
unsafe impl<V: Send, S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>> Send for SimpleLPHashMap<V, S> {}

impl<V, S> SimpleLPHashMap<V, S>
where 
    S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>
{
    /// Create from pre-existing storage.
    /// 
    /// # Safety
    /// - Storage must be initialized with valid `Slot<V>` values
    /// - Capacity must be a power of two
    pub unsafe fn from_storage(storage: S) -> Self {
        let capacity = storage.capacity();
        assert!(capacity.is_power_of_two() && capacity > 0);
        Self {
            mask: capacity - 1,
            slots: storage,
            phantom: core::marker::PhantomData,
        }
    }

    #[inline]
    fn slots(&self) -> &[Slot<V>] {
        self.slots.slice()
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
impl<V> SimpleLPHashMap<V, BoxedSliceStorage<Slot<V>>> {
    /// Create a new map with Vec-backing and the given capacity. Capacity must be a power of two.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two() && capacity > 0);
        Self {
            mask: capacity - 1,
            slots: BoxedSliceStorage::with_capacity_and_init(capacity, Slot::new),
            phantom: core::marker::PhantomData,
        }
    }

    /// Create a map with custom initialization for all values.
    /// Capacity must be a power of two.
    /// Keys are initialized to EMPTY (0).
    pub fn with_capacity_and_init<F>(capacity: usize, mut init_value: F) -> Self
    where
        F: FnMut() -> V,
    {
        // Not 'assert!' since `area.rs` already checks this.
        debug_assert!(capacity.is_power_of_two() && capacity > 0);
        Self {
            mask: capacity - 1,
            slots: BoxedSliceStorage::with_capacity_and_init(capacity, || Slot {
                key: AtomicU64::new(EMPTY),
                value: UnsafeCell::new(MaybeUninit::new(init_value())),
            }),
            phantom: core::marker::PhantomData,
        }
    }
}

impl<V, S> SimpleLPHashMap<V, S>
where 
    S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>
{
    /// Get the capacity of the map
    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Get raw access to all slots for specialized scanning
    ///
    /// # Safety
    /// - The caller must ensure proper synchronization when accessing slots
    /// - The caller must handle EMPTY keys and IN_PROGRESS state appropriately
    #[inline]
    pub unsafe fn raw_slots(&self) -> &[Slot<V>] {
        &self.slots()
    }

    /// Get a const reference to a slot by index
    ///
    /// # Safety
    /// - index must be < capacity
    #[inline]
    pub unsafe fn slot_at(&self, index: usize) -> &Slot<V> {
        &self.slots()[index]
    }

    /// Scan all slots and call the visitor function for each non-empty slot.
    /// The visitor receives (index, key, value_ptr).
    ///
    /// # Safety
    /// - The caller must ensure proper synchronization when accessing value_ptr
    /// - The caller must handle IN_PROGRESS state appropriately
    pub unsafe fn scan_slots<F>(&self, mut visitor: F)
    where
        F: FnMut(usize, u64, *const V),
    {
        for (index, slot) in self.slots().iter().enumerate() {
            let k = slot.load_key();
            if k != EMPTY {
                let ptr = self.value_ptr(index);
                visitor(index, k, ptr);
            }
        }
    }

    /// Perform (power-of-two) modulo on key to get index into slots array.
    /// 
    /// Implicitly strips IN_PROGRESS bit since it is MSB.
    #[inline]
    fn index_for(&self, key: u64, placement_offset: usize) -> usize {
        ((key as usize) + placement_offset) & self.mask
    }

    /// Get a raw pointer to the value at index.
    #[inline]
    fn value_ptr(&self, index: usize) -> *const V {
        unsafe { (*self.slots()[index].value.get()).as_ptr() }
    }

    /// Concurrent get-or-insert using CAS on the key.
    /// 
    /// Importantly, the n probing mechanism cannot be used for insertions where there is a possibility 
    /// of the key already existing, as that could create duplicate entries.
    /// Only attempt it if you know what you are doing.
    /// If the key is not required to be specifically the one provided however and no duplicate handling 
    /// is required, specify `fold=true` to allow generation of alternate unique keys. When fold is set, 
    /// assume that the inserted entry's key is not the one initially provided, but `key_bits(k)` instead with a placement_offset of zero.
    /// When fold = true, concurrent calls with the same (key, placement_offset) are allowed and may all insert distinct entries. This mode does not provide "get-or-insert" uniqueness; it only guarantees each successful insertion gets a unique key.
    /// See examples below.
    ///
    /// Returns `(ptr, is_new, placement_offset_delta, k, index)`:
    /// - If `ptr` is null, when `insert == true` the table is full and nothing was inserted, and when `insert == false` the key was not found.
    /// - If `is_new` is true:
    ///     - This call just claimed the slot.
    ///     - The key in the slot is `k`, which is:
    ///         - `key | IN_PROGRESS` when `fold == false`, or
    ///         - a folded variant as described above when `fold == true`.
    ///     - You must initialize `*ptr`, then call `finish_init_at(index)`.
    /// - If `is_new` is false and `ptr` is non-null:
    ///     - The key already exists in the table.
    ///     - `is_in_progress(k)` tells you whether the slot is still being initialized
    ///       by some thread (IN_PROGRESS bit set).
    /// - `placement_offset_delta` is the number of extra probe steps from index_for(key, placement_offset) before returning.
    /// - `k` is the actual key stored in the slot (the IN_PROGRESS bit might be set; use `key_bits(k)` to retrieve the actual key).
    /// 
    /// # Safety
    /// - Keys must never be `0`.
    /// - If `is_new` is true, you must initialize `*ptr` exactly once.
    /// - You must not read from `*ptr` while another thread is still
    ///   initializing (signaled by `is_in_progress(k) == true`).
    /// - You must eventually call `finish_init_at(index)` to clear the flag.
    /// - All races on the *contents* of `V` are the caller’s responsibility.
    /// 
    /// # Examples
    /// Usage pattern for inserting a unique new entry with non-specific key:
    /// ```
    /// use market_square::map::{SimpleLPHashMap, key_bits, ZERO_OFFSET, IMMEDIATE};
    /// use std::sync::atomic::{AtomicU64, Ordering};
    ///
    /// let map = SimpleLPHashMap::<AtomicU64, _>::with_capacity(16);
    /// let seed_key = 12345u64;
    /// let placement_offset = ZERO_OFFSET;
    /// let n = 16;
    ///
    /// unsafe {
    ///     let (ptr, is_new, delta, k, index) = map.get_or_insert_concurrent(seed_key, placement_offset, n, true, true);
    ///
    ///     // After this returns with is_new == true, the real key is:
    ///     let real_key = key_bits(k);
    ///
    ///     // Initialize the value
    ///     if is_new {
    ///         unsafe { (&*(ptr)).store(42, Ordering::Relaxed); } // Relaxed is alright here since `finish_init_at` does a Release
    ///         map.finish_init_at(index);
    ///     }
    ///
    ///     // Store real_key somewhere as the handle; never use seed_key again for this entry.
    ///
    ///     // Lookup
    ///     let (ptr, is_new, _, k, _) = map.get_or_insert_concurrent(real_key, ZERO_OFFSET, IMMEDIATE, false, false);
    ///     debug_assert!(!is_new); // If you've kept your handles straight.
    ///
    ///     // Removal
    ///     map.remove_concurrent(real_key, ZERO_OFFSET, IMMEDIATE);
    /// }
    /// ```
    pub unsafe fn get_or_insert_concurrent(
        &self,
        key: u64,
        placement_offset: usize,
        mut n: usize,
        insert: bool,
        fold: bool,
    ) -> (*const V, bool, usize, u64, usize) {
        debug_assert!(key != EMPTY, "key must be non-zero");
        debug_assert!(key & IN_PROGRESS == 0, "keys must have MSB clear");
        debug_assert!(
            insert || !fold,
            "fold has no effect when insert == false; this is probably a bug"
        );

        let mut idx = self.index_for(key, placement_offset);

        if n == MAX {
            n = self.mask + 1;
        }

        for i in 0..n {
            let k = self.slots()[idx].load_key();

            if insert && k == EMPTY {
                let expected = k;
                let new_key = if !fold { key | IN_PROGRESS } else {
                    // Generate a unique key by folding in the index.
                    // This is only valid if the caller does not require
                    // the key to be exactly the one provided.
                    let mut logical_key = key.wrapping_add(placement_offset as u64).wrapping_add(i as u64);
                    if logical_key == 0 || logical_key == IN_PROGRESS {
                        logical_key = logical_key.wrapping_add((self.mask + 1) as u64);
                    }
                    logical_key | IN_PROGRESS
                };
                match self.slots()[idx].key.compare_exchange(
                    expected,
                    new_key,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // We claimed this slot; value is currently uninitialized.
                        return (self.value_ptr(idx), true, i, new_key, idx);
                    }
                    Err(actual) => {
                        // Slot changed under us; if it became our key, just use it. (only in non-folding mode, as folding mode does not provide get-or-insert semantics by design)
                        if !fold && actual != EMPTY && key_bits(actual) == key {
                            let ptr = self.value_ptr(idx);
                            return (ptr, false, i, actual, idx);
                        }
                        // Otherwise keep probing.
                    }
                }
            }

            if !fold && key_bits(k) == key {
                // Found existing key.
                let ptr = self.value_ptr(idx);
                return (ptr, false, i, k, idx);
            }

            idx = (idx + 1) & self.mask;
        }

        // Item not found if insert==false or table is fully occupied (no EMPTY and no slot for this key) if insert==true.
        (ptr::null_mut(), false, usize::MAX, EMPTY, usize::MAX)
    }

    /// Clear the IN_PROGRESS bit at `index` once initialization is complete.
    ///
    /// # Safety
    /// - `index` must refer to a slot that is owned by the caller and was
    ///   returned from `get_or_insert_concurrent`.
    /// - The value at that slot must be fully initialized before calling this.
    pub unsafe fn finish_init_at(&self, index: usize) {
        let slot = &self.slots()[index];
        let k = slot.key.load(Ordering::Acquire);
        debug_assert!(k != EMPTY);
        if is_in_progress(k) {
            // Clear IN_PROGRESS bit, publishing the initialized value.
            slot.key.fetch_and(!IN_PROGRESS, Ordering::Release);
        }
    }

    /// Concurrently mark the key's slot as EMPTY, without touching the value.
    ///
    /// # Safety
    /// - Must only be called when this thread has exclusive logical ownership
    ///   of the key (no other threads using this key/slot).
    /// - Caller must have logically dropped the value first if `V` needs drop.
    pub unsafe fn remove_concurrent(&self, key: u64, placement_offset: usize, mut n: usize) -> bool {
        debug_assert!(key != EMPTY, "key must be non-zero");
        debug_assert!(key & IN_PROGRESS == 0, "keys must have MSB clear");

        let mut idx = self.index_for(key, placement_offset);

        if n == MAX {
            n = self.mask + 1;
        }

        for _ in 0..n {
            let k = self.slots()[idx].load_key();
            
            if key_bits(k) == key {
                if self.slots()[idx].key.compare_exchange(
                    k,
                    EMPTY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ).is_ok() {
                    return true;
                } else {
                    // In correct use (exclusive access for this key), this shouldn't happen.
                    return false;
                }
            }

            idx = (idx + 1) & self.mask;
        }

        false
    }
}

impl<V, S> Drop for SimpleLPHashMap<V, S>
where 
    S: FixedStorage<Slot<V>> + FixedStorageMultiple<Slot<V>>
{
    fn drop(&mut self) {
        unsafe {
            self.slots.deallocate();
        }
    }
}
