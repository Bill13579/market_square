use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
#[cfg(all(feature = "alloc", not(feature = "std")))]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std as alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crossbeam_utils::CachePadded;
use rand::Rng;

use crate::map::{self, IMMEDIATE, SimpleLPHashMap, ZERO_OFFSET};

// MSB is reserved for "suspended" status on reader generation numbers
const SUSPENDED_BIT: u64 = 1 << 63;

/// Strip the suspended bit from a generation number
#[inline]
pub fn gen_without_suspended_bit(generation: u64) -> u64 {
    generation & !SUSPENDED_BIT
}

/// Check if a generation number has the suspended bit set
#[inline]
pub fn is_suspended(generation: u64) -> bool {
    (generation & SUSPENDED_BIT) != 0
}

/// Errors that can occur when reserving slots
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// No space available in the ring buffer (capacity check failed)
    NoSpace,
    /// Failed to grab slots (race condition - retry might succeed)
    FailedGrab,
}

/// Errors that can occur when publishing slots
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    /// The range doesn't match the current read_gen (someone else published in between, or someone else with earlier generations have not yet published)
    CasFailed,
}

/// Errors that can occur when setting reader generation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetGenError {
    /// Reader ID not found in the map
    ReaderNotFound,
    /// Reader is suspended, resume first to gain exclusive access before trying to set the generation
    ReaderSuspended,
    /// New generation is older than current generation; older generations are not guaranteed to still exist, and causes undefined behavior
    InvalidGeneration,
}

/// Errors that can occur when suspending a reader
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendError {
    /// Reader ID not found in the map
    ReaderNotFound,
    /// The reader is already suspended
    AlreadySuspended,
}

/// Errors that can occur when resuming a reader
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeError {
    /// Reader ID not found in the map
    ReaderNotFound,
    /// The requested entry generation is older than the current forced minimum
    GenerationTooOld,
}

/// Errors that can occur when registering a reader
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// The reader map is full (capacity reached)
    ReaderCapacityReached,
}

/// Errors that can occur when reading messages.
///
/// Messages could not be retrieved because there are no writers in the area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadError;

/// Errors that can occur when unregistering a reader
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnregisterError {
    /// Reader ID not found in the map
    ReaderNotFound,
}

/// Errors that can occur during cleanup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupError {
    /// No cleanup needed
    NothingToClean,
}

/// Result of a successful cleanup operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupResult {
    /// The old last_valid_gen before cleanup
    pub old_last_valid: u64,
    /// The new last_valid_gen after cleanup
    pub new_last_valid: u64,
    /// Number of slots cleaned
    pub slots_cleaned: u64,
}

/// The shared inner state of an Area
struct AreaInner<T> {
    /// Next generation number for writing (monotonically increasing)
    write_gen: CachePadded<AtomicU64>,

    /// Current generation number published for reading (monotonically increasing)
    read_gen: CachePadded<AtomicU64>,

    /// Last generation number that is still valid (for cleanup)
    last_valid_gen: CachePadded<AtomicU64>,

    /// Generation numbers that are now freed and can be reused
    free_gen: CachePadded<AtomicU64>,

    /// Number of active writers
    writers_count: CachePadded<AtomicUsize>,

    /// Per-reader generation numbers (with MSB as suspended flag)
    /// Each value is CachePadded<AtomicU64> for cache-line isolation
    reader_gens: SimpleLPHashMap<CachePadded<AtomicU64>>,

    /// Ring buffer capacity
    buffer_capacity: usize,

    /// Ring buffer storage
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,

    /// Destruction stages counter (leaked, separately allocated)
    /// Starts at 2 (1 for writers, 1 for readers collectively)
    destroy_stages: NonNull<AtomicUsize>,

    /// Reader keep-alloc tickets (leaked, separately allocated)
    /// Used to gate non-competing reader destructors
    reader_keep_alloc_tickets: NonNull<AtomicUsize>,

    /// Reader stage tickets (leaked, separately allocated)
    /// Count of readers competing to decrement destroy_stages
    reader_stage_tickets: NonNull<AtomicUsize>,
}

unsafe impl<T: Send> Send for AreaInner<T> {}
unsafe impl<T: Send> Sync for AreaInner<T> {}

impl<T> AreaInner<T> {
    /// Create a new AreaInner with the given buffer capacity and reader capacity
    fn new(buffer_capacity: usize, reader_capacity: usize) -> NonNull<Self> {
        assert!(buffer_capacity > 0, "buffer_capacity must be > 0");
        assert!(
            reader_capacity.is_power_of_two(),
            "reader_capacity must be a power of two"
        );
        assert!(reader_capacity > 0, "reader_capacity must be > 0");

        let mut buffer_vec = Vec::with_capacity(buffer_capacity);
        for _ in 0..buffer_capacity {
            buffer_vec.push(UnsafeCell::new(MaybeUninit::uninit()));
        }

        // Allocate and leak the destruction coordination atomics
        let destroy_stages = Box::leak(Box::new(AtomicUsize::new(2)));
        let reader_keep_alloc_tickets = Box::leak(Box::new(AtomicUsize::new(0)));
        let reader_stage_tickets = Box::leak(Box::new(AtomicUsize::new(0)));

        let inner = Self {
            write_gen: CachePadded::new(AtomicU64::new(0)),
            read_gen: CachePadded::new(AtomicU64::new(0)),
            last_valid_gen: CachePadded::new(AtomicU64::new(0)),
            free_gen: CachePadded::new(AtomicU64::new(0)),
            writers_count: CachePadded::new(AtomicUsize::new(0)),
            reader_gens: SimpleLPHashMap::with_capacity_and_init(reader_capacity, || {
                CachePadded::new(AtomicU64::new(SUSPENDED_BIT))
            }),
            buffer_capacity,
            buffer: buffer_vec.into_boxed_slice(),
            destroy_stages: NonNull::from(destroy_stages),
            reader_keep_alloc_tickets: NonNull::from(reader_keep_alloc_tickets),
            reader_stage_tickets: NonNull::from(reader_stage_tickets),
        };

        // Leak the AreaInner and return a NonNull pointer
        NonNull::from(Box::leak(Box::new(inner)))
    }

    /// Get a mutable pointer to the slot at the given generation number
    #[inline]
    unsafe fn get_slot_ptr_mut(&self, generation: u64) -> *mut T {
        let index = (generation as usize) % self.buffer_capacity;
        unsafe { (*self.buffer[index].get()).as_mut_ptr() }
    }

    /// Get a const pointer to the slot at the given generation number
    #[inline]
    unsafe fn get_slot_ptr_const(&self, generation: u64) -> *const T {
        let index = (generation as usize) % self.buffer_capacity;
        unsafe { (*self.buffer[index].get()).as_ptr() }
    }

    /// Load the current write generation (non-inclusive)
    #[inline]
    fn load_write_gen(&self) -> u64 {
        self.write_gen.load(Ordering::Acquire)
    }

    /// Load the current read generation (non-inclusive)
    #[inline]
    fn load_read_gen(&self) -> u64 {
        self.read_gen.load(Ordering::Acquire)
    }

    /// Load the current last valid generation (inclusive)
    #[inline]
    fn load_last_valid_gen(&self) -> u64 {
        self.last_valid_gen.load(Ordering::Acquire)
    }

    /// Load the current free generation (non-inclusive)
    #[inline]
    fn load_free_gen(&self) -> u64 {
        self.free_gen.load(Ordering::Acquire)
    }

    /// Load the number of active writers
    #[inline]
    fn load_writers_count(&self) -> usize {
        self.writers_count.load(Ordering::Acquire)
    }

    /// Try to reserve n slots for writing using CAS.
    /// Returns (start_generation, end_generation) on success.
    /// end_generation is exclusive (so the range is [start_generation, end_generation))
    fn try_reserve_slots(&self, n: usize) -> Result<(u64, u64), ReserveError> {
        debug_assert!(n != 0, "must reserve at least one slot");

        // Load current state
        let current_free = self.load_free_gen();
        let current_write = self.load_write_gen(); //NOTE: Must come AFTER loading current_free to be conservative about free spots being available.
        debug_assert!(current_free <= current_write, "free_gen should never exceed write_gen! this should never happen. {} {} {}", current_free, current_write, self.load_read_gen());
        let available = self.buffer_capacity as u64 - (self.buffer_capacity as u64).min(current_write - current_free); //NOTE: If current_write - current_free > buffer_capacity, that's because current_free hasn't caught up yet, since we never allow writers to lap readers by more than buffer_capacity. In that case, be conservative and report 0 available slots.

        // Check if there's enough capacity
        if (n as u64) > available {
            return Err(ReserveError::NoSpace);
        }

        // Try to CAS write_gen from current_write to current_write + n
        let expected_new_write = current_write + n as u64;
        match self.write_gen.compare_exchange(
            current_write,
            expected_new_write,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok((current_write, expected_new_write)),
            Err(_) => Err(ReserveError::FailedGrab),
        }
    }

    /// Try to reserve up to n slots, getting as many as possible, using CAS.
    /// Returns (start_generation, end_generation, actual_count) or FailedGrab on race.
    /// Never returns NoSpace - gets 0 slots if none available.
    fn try_reserve_slots_best_effort(&self, n: usize) -> Result<(u64, u64, usize), ReserveError> {
        if n == 0 {
            return Ok((0, 0, 0));
        }

        // Load current state
        let current_free = self.load_free_gen();
        let current_write = self.load_write_gen(); //NOTE: Must come AFTER loading current_free to be conservative about free spots being available.
        debug_assert!(current_free <= current_write, "free_gen should never exceed write_gen! this should never happen. {} {} {}", current_free, current_write, self.load_read_gen());
        let available = self.buffer_capacity as u64 - (self.buffer_capacity as u64).min(current_write - current_free); //NOTE: If current_write - current_free > buffer_capacity, that's because current_free hasn't caught up yet, since we never allow writers to lap readers by more than buffer_capacity. In that case, be conservative and report 0 available slots.
        let actual_n = (n as u64).min(available);

        if actual_n == 0 {
            return Ok((0, 0, 0));
        }

        // Try to CAS write_gen from current_write to current_write + actual
        let expected_new_write = current_write + actual_n;
        match self.write_gen.compare_exchange(
            current_write,
            expected_new_write,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok((current_write, expected_new_write, actual_n as usize)),
            Err(_) => Err(ReserveError::FailedGrab),
        }
    }

    /// Publish slots in the range [start_generation, end_generation) for readers.
    /// This must be called with exactly the range returned by try_reserve_slots.
    /// Uses CAS to ensure no one else published in between.
    fn publish_slots(
        &self,
        start_generation: u64,
        end_generation: u64,
    ) -> Result<(), PublishError> {
        if start_generation >= end_generation {
            return Ok(());
        }

        // CAS read_gen from start_generation to end_generation
        match self.read_gen.compare_exchange(
            start_generation,
            end_generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(PublishError::CasFailed),
        }
    }

    /// Register a new reader, returning a unique reader ID.
    /// Uses CAS to enter from suspended state, retries on CAS failure.
    fn register_reader(&self) -> Result<u64, RegisterError> {
        // Generate a random reader ID using thread RNG
        let mut rng = rand::rng();
        let mut seed = rng.random::<u64>();

        // Ensure seed doesn't have MSB set
        seed = seed & !(1 << 63);
        if seed == 0 {
            seed = 1;
        }

        // Use folded insertion to get a unique reader ID
        unsafe {
            let (ptr, is_new, _, reader_id_raw, index): (*const CachePadded<AtomicU64>, _, _, _, _) = self.reader_gens.get_or_insert_concurrent(
                seed,
                ZERO_OFFSET,
                self.reader_gens.capacity(),
                true,
                true, // fold = true for unique ID generation
            );

            if ptr.is_null() {
                return Err(RegisterError::ReaderCapacityReached);
            }

            debug_assert!(is_new, "fold mode should always create new entries");

            // Strip the IN_PROGRESS bit to get the actual reader ID
            let reader_id = map::key_bits(reader_id_raw);

            // Ensure the reader ID is valid (no MSB set)
            debug_assert!(reader_id != 0 && (reader_id & (1 << 63)) == 0);

            // Initialize the CachePadded<AtomicU64>
            // Pre-initialized value is SUSPENDED_BIT (or dragged forward by cleanup), we'll CAS it to entry generation
            // We MUST NOT overwrite the value here, as it may contain a forced update from cleanup.
            // ptr.write(CachePadded::new(AtomicU64::new(SUSPENDED_BIT)));
            self.reader_gens.finish_init_at(index);

            // Now CAS from suspended state to active, like resume does
            // We loop here because if CAS fails, it just means cleanup moved us forward, so we should just accept the new reality
            loop {
                let current = (**ptr).load(Ordering::Acquire);
                debug_assert!(is_suspended(current), "new slot should be suspended");

                // Entry generation is the current value without the MSB
                let entry_generation = gen_without_suspended_bit(current);

                // CAS to enter active state
                match (**ptr).compare_exchange(
                    current,
                    entry_generation,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(reader_id),
                    Err(_) => continue, // Retry with new current value
                }
            }
        }
    }

    #[inline(always)]
    fn get_reader_ptr_boilerplate(
        &self,
        reader_id: u64,
        func_name: &str,
    ) -> Option<*const CachePadded<AtomicU64>> {
        unsafe {
            let (ptr, is_new, _, _, _): (*const CachePadded<AtomicU64>, _, _, _, _) = self.reader_gens.get_or_insert_concurrent(
                reader_id,
                ZERO_OFFSET,
                IMMEDIATE,
                false,
                false,
            );

            if ptr.is_null() {
                return None;
            }

            debug_assert!(
                !is_new,
                "{} should not be used to create new readers",
                func_name
            );

            // No need to wait for initialization here since by definition of folded keys, if the caller knows the key, `register_reader` must have completed, meaning it must be initialized.
            // In fact, since the items are all atomic integers anyways, initialization is just a formality.

            Some(ptr)
        }
    }

    /// Get the current generation number for a reader; also might include the suspended bit
    fn get_reader_gen(&self, reader_id: u64) -> Option<u64> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "get_reader_gen")?;

            let generation = (**ptr).load(Ordering::Acquire);
            Some(generation)
        }
    }

    /// Try to set the reader's generation number (for advancing); fails if the reader is suspended
    fn set_reader_gen(&self, reader_id: u64, new_generation: u64) -> Result<(), SetGenError> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "set_reader_gen").ok_or(SetGenError::ReaderNotFound)?;

            let current = (**ptr).load(Ordering::Acquire);

            // If suspended, we DO NOT own the slot anymore (cleanup thread might be updating it),
            // so we must fail.
            if is_suspended(current) {
                return Err(SetGenError::ReaderSuspended);
            }

            // Don't allow setting to a value less than current (this is non-negotiable, since the contract is that readers acknowledge any generations older than their current stated generation is not guaranteed to still exist)
            if new_generation < current {
                return Err(SetGenError::InvalidGeneration);
            }

            // For simplicity, just do a simple store since reader owns this
            (**ptr).store(new_generation, Ordering::Release);
            Ok(())
        }
    }

    /// Mark a reader as suspended by setting the MSB on their generation number
    fn suspend_reader(&self, reader_id: u64) -> Result<(), SuspendError> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "suspend_reader").ok_or(SuspendError::ReaderNotFound)?;

            let current = (**ptr).load(Ordering::Acquire);

            // If already suspended, do nothing.
            // This is important because if it IS suspended, the cleanup thread might own it
            // and be in the process of updating it. We shouldn't overwrite that.
            if is_suspended(current) {
                return Err(SuspendError::AlreadySuspended);
            }

            let suspended = current | SUSPENDED_BIT;
            (**ptr).store(suspended, Ordering::Release);

            Ok(())
        }
    }

    /// Resume a reader from suspended state at the current valid generation.
    /// Retries on CAS failure.
    fn resume_reader(&self, reader_id: u64) -> Result<bool, ResumeError> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "resume_reader").ok_or(ResumeError::ReaderNotFound)?;

            loop {
                let current = (**ptr).load(Ordering::Acquire);

                if !is_suspended(current) {
                    // Not suspended, nothing to do
                    return Ok(false);
                }

                // We just want to clear the suspended bit, whatever the current generation is
                let expected = current;
                let desired = current & !SUSPENDED_BIT;

                match (**ptr).compare_exchange(
                    expected,
                    desired,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(true),
                    Err(_) => continue, // Retry
                }
            }
        }
    }

    /// Try to resume a reader from suspended state at a specific entry_generation.
    /// Fails if entry_generation is older than the current forced minimum.
    /// Retries on CAS failure.
    fn resume_reader_at(&self, reader_id: u64, entry_generation: u64) -> Result<bool, ResumeError> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "resume_reader_at").ok_or(ResumeError::ReaderNotFound)?;

            loop {
                let current = (**ptr).load(Ordering::Acquire);

                if !is_suspended(current) {
                    // Not suspended, nothing to do
                    return Ok(false);
                }

                // Validate that entry_generation is at least the current value (without suspend bit)
                let current_gen_without_suspend = gen_without_suspended_bit(current);
                if entry_generation < current_gen_without_suspend {
                    return Err(ResumeError::GenerationTooOld);
                }

                let expected = current;
                let desired = entry_generation & !SUSPENDED_BIT; // Clear suspended bit

                match (**ptr).compare_exchange(
                    expected,
                    desired,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(true),
                    Err(_) => continue, // Retry
                }
            }
        }
    }

    /// Unregister a reader, removing their entry from the reader_gens map
    fn unregister_reader(&self, reader_id: u64) -> Result<(), UnregisterError> {
        // Suspend the reader first so that if the slot is reused or scanned by cleanup,
        // it holds a safe value (SUSPENDED_BIT).
        match self.suspend_reader(reader_id) {
            Ok(_) | Err(SuspendError::AlreadySuspended) => {},
            Err(SuspendError::ReaderNotFound) => return Err(UnregisterError::ReaderNotFound),
        }
        unsafe {
            // No concurrency problems with the value since the private generation number remains valid no matter if a key is associated with it currently or not; initialization is a formality.
            self.reader_gens
                .remove_concurrent(reader_id, ZERO_OFFSET, IMMEDIATE);
        }
        Ok(())
    }

    /// Try to cleanup old slots by advancing last_valid_gen.
    /// This scans ALL reader generation slots (including empty ones), force-advances suspended readers,
    /// and attempts to update last_valid_gen via CAS.
    fn try_cleanup_old_slots(&self) -> Result<CleanupResult, CleanupError> {
        let mut last_valid = self.load_last_valid_gen();
        let read_gen = self.load_read_gen();

        if last_valid >= read_gen {
            return Err(CleanupError::NothingToClean);
        }

        // Find minimum held generation across ALL reader slots
        let mut min_held_gen = read_gen;

        // Scan ALL slots in reader_gens map (including empty ones)
        unsafe {
            let raw_slots = self.reader_gens.raw_slots();
            'outer: for slot in raw_slots {
                // Get the value pointer
                let ptr = (*slot.value.get()).as_ptr();

                // No need to check for initialization here since the items (associated with a key or not) are all atomic integers anyways; initialization state is just a formality.

                // Load the reader's current generation
                // Even EMPTY slots have values (pre-initialized to SUSPENDED_BIT)
                let mut reader_gen = (**ptr).load(Ordering::Acquire);

                // While the reader is suspended (or slot is pre-initialized), try to force-advance it
                'inner: while is_suspended(reader_gen) {
                    let current_gen_without_suspend = gen_without_suspended_bit(reader_gen);

                    // The new re-entry minimum we want to advance suspended readers to
                    let forced_update = min_held_gen.max(current_gen_without_suspend);

                    // Try to CAS the reader's gen to the forced update with suspended bit
                    match (**ptr).compare_exchange(
                        reader_gen,
                        forced_update | SUSPENDED_BIT,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // If we succeed in forcing the update, we are done with this slot.
                            continue 'outer;
                        },
                        Err(actual) => {
                            // If we fail in forcing the update, either the reader was unsuspended and we need to treat it as normal, or the reader is still suspended and we can retry.
                            reader_gen = actual;
                            continue 'inner;
                        }
                    }
                }

                // Reader is active, so track their generation
                min_held_gen = min_held_gen.min(reader_gen);
            }
        }

        let new_last_valid = min_held_gen;

        // Loop while we can make progress
        loop {
            if new_last_valid <= last_valid {
                // Either there was nothing to clean, or if we are on the second iteration and beyond, other threads have cleaned everything we wanted to clean.
                return Err(CleanupError::NothingToClean);
            }
            // Since new_last_valid > last_valid/actual, there's more to clean, so try to do that.

            // Try to CAS last_valid_gen:
            match self.last_valid_gen.compare_exchange(
                last_valid,
                new_last_valid,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully updated. Now we need to drop items in [last_valid, new_last_valid)
                    let slots_cleaned = new_last_valid - last_valid;

                    // Drop items if T needs drop
                    if core::mem::needs_drop::<T>() {
                        for generation in last_valid..new_last_valid {
                            unsafe {
                                let ptr = self.get_slot_ptr_mut(generation);
                                ptr::drop_in_place(ptr);
                            }
                        }
                    }

                    // Update free_gen to allow writers to reuse the slots we just freed
                    loop {
                        match self.free_gen.compare_exchange(
                            last_valid,
                            new_last_valid,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break,
                            Err(_) => core::hint::spin_loop(), // Loop again to try updating free_gen since another cleanup thread is still working
                        }
                    }

                    return Ok(CleanupResult {
                        old_last_valid: last_valid,
                        new_last_valid,
                        slots_cleaned,
                    });
                }
                Err(actual) => {
                    last_valid = actual;
                    // Loop again to try advancing further
                    // continue;
                }
            }
        }
    }

    /// Check if there are any readers in the map
    fn has_readers(&self) -> bool {
        unsafe {
            let raw_slots = self.reader_gens.raw_slots();
            for slot in raw_slots {
                let k = slot.key.load(Ordering::Acquire);

                if k != map::EMPTY {
                    return true;
                }
            }
        }
        false
    }
}

/// A writer handle for an Area
#[derive(Debug)]
pub struct AreaWriter<T> {
    inner: NonNull<AreaInner<T>>,
}

unsafe impl<T: Send> Send for AreaWriter<T> {}
unsafe impl<T: Send> Sync for AreaWriter<T> {}

impl<T> AreaWriter<T> {
    /// Create a new writer handle for the same area
    #[inline]
    pub fn create_writer(&self) -> Self {
        unsafe {
            let inner = self.inner.as_ref();
            inner.writers_count.fetch_add(1, Ordering::AcqRel);
            AreaWriter { inner: self.inner }
        }
    }
}

pub trait AreaWriterTrait<T> {
    /// Try to reserve n slots for writing.
    /// Returns (start_generation, end_generation) on success.
    /// end_generation is exclusive (so the range is [start_generation, end_generation)).
    fn try_reserve_slots(&self, n: usize) -> Result<(u64, u64), ReserveError>;

    /// Try to reserve up to n slots, getting as many as possible.
    /// Returns (start_generation, end_generation, actual_count) or FailedGrab on race.
    /// Never returns NoSpace - gets 0 slots if none available.
    /// end_generation is exclusive (so the range is [start_generation, end_generation)).
    fn try_reserve_slots_best_effort(
        &self,
        n: usize,
    ) -> Result<(u64, u64, usize), ReserveError>;

    /// Publish slots in the range [start_generation, end_generation) for readers.
    fn publish_slots(
        &self,
        start_generation: u64,
        end_generation: u64,
    ) -> Result<(), PublishError>;

    /// Get a mutable pointer to the slot at the given generation number.
    ///
    /// # Safety
    /// - generation must be within a range you reserved via try_reserve_slots
    /// - You must initialize the slot before calling publish_slots
    /// - You MUST NOT access the slot after publishing (use a separate AreaReader for that)
    unsafe fn get_slot_ptr(&self, generation: u64) -> *mut T;
}

macro_rules! impl_writer_functionality {
    ( $($name:ident),* ) => { // Matches one or more identifiers separated by commas
        $( // Starts a repetition for each identifier captured by `$name`
            impl<T> AreaWriterTrait<T> for $name<T> {
                /// Try to reserve n slots for writing.
                /// Returns (start_generation, end_generation) on success.
                /// end_generation is exclusive (so the range is [start_generation, end_generation)).
                #[inline]
                fn try_reserve_slots(&self, n: usize) -> Result<(u64, u64), ReserveError> {
                    unsafe { self.inner.as_ref().try_reserve_slots(n) }
                }

                /// Try to reserve up to n slots, getting as many as possible.
                /// Returns (start_generation, end_generation, actual_count) or FailedGrab on race.
                /// Never returns NoSpace - gets 0 slots if none available.
                /// end_generation is exclusive (so the range is [start_generation, end_generation)).
                #[inline]
                fn try_reserve_slots_best_effort(
                    &self,
                    n: usize,
                ) -> Result<(u64, u64, usize), ReserveError> {
                    unsafe { self.inner.as_ref().try_reserve_slots_best_effort(n) }
                }

                /// Publish slots in the range [start_generation, end_generation) for readers.
                #[inline]
                fn publish_slots(
                    &self,
                    start_generation: u64,
                    end_generation: u64,
                ) -> Result<(), PublishError> {
                    unsafe {
                        self.inner
                            .as_ref()
                            .publish_slots(start_generation, end_generation)
                    }
                }

                /// Get a mutable pointer to the slot at the given generation number.
                ///
                /// # Safety
                /// - generation must be within a range you reserved via try_reserve_slots
                /// - You must initialize the slot before calling publish_slots
                /// - You MUST NOT access the slot after publishing (use a separate AreaReader for that)
                #[inline]
                unsafe fn get_slot_ptr(&self, generation: u64) -> *mut T {
                    unsafe { self.inner.as_ref().get_slot_ptr_mut(generation) }
                }
            }

            impl<T> $name<T> {
                /// Create a reservation for n slots.
                /// Returns a RAII guard that must be published.
                pub fn reserve(&self, n: usize) -> Result<Reservation<'_, Self, T>, ReserveError> {
                    let (start, end) = self.try_reserve_slots(n)?;
                    Ok(Reservation {
                        writer: self,
                        start_gen: start,
                        end_gen: end,
                        published: false,
                        phantom: PhantomData,
                    })
                }

                /// Try to reserve up to n slots, getting as many as possible.
                /// Returns a RAII guard that must be published.
                pub fn reserve_best_effort(&self, n: usize) -> Result<Reservation<'_, Self, T>, ReserveError> {
                    let (start, end, count) = self.try_reserve_slots_best_effort(n)?;
                    debug_assert!(end - start == count as u64);
                    Ok(Reservation {
                        writer: self,
                        start_gen: start,
                        end_gen: end,
                        published: false,
                        phantom: PhantomData,
                    })
                }
            }

            impl<T> $name<T> {
                /// Get the number of active writers
                pub fn get_writers_count(&self) -> usize {
                    unsafe { self.inner.as_ref().load_writers_count() }
                }
            }
        )* // Ends the repetition
    };
}

impl_writer_functionality!(AreaWriter, AreaReader);

impl<T> AreaWriter<T> {
    /// Try to cleanup old slots by advancing last_valid_gen
    #[inline]
    pub fn try_cleanup_old_slots(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.inner.as_ref().try_cleanup_old_slots() }
    }
}

impl<T> Drop for AreaWriter<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ref();

            // Decrement writers_count
            let prev_count = inner.writers_count.fetch_sub(1, Ordering::AcqRel);

            // If we were the last writer, decrement destroy_stages
            if prev_count == 1 {
                let destroy_stages = inner.destroy_stages.as_ref();
                let prev_stages = destroy_stages.fetch_sub(1, Ordering::AcqRel);

                // If we got destroy_stages to 0, we're responsible for cleanup
                if prev_stages == 1 {
                    // Drop all remaining items in the buffer
                    let _ = inner.try_cleanup_old_slots();

                    // Free the AreaInner
                    let inner_box = Box::from_raw(self.inner.as_ptr());

                    // Free the leaked atomics
                    let _ = Box::from_raw(inner_box.destroy_stages.as_ptr());
                    let _ = Box::from_raw(inner_box.reader_keep_alloc_tickets.as_ptr());
                    let _ = Box::from_raw(inner_box.reader_stage_tickets.as_ptr());

                    // inner_box is dropped here, freeing AreaInner
                }
            }
        }
    }
}

/// A RAII guard for reserved slots.
/// Must be published or dropped (which panics if not published).
#[derive(Debug)]
pub struct Reservation<'a, W, T>
where 
    W: AreaWriterTrait<T>
{
    writer: &'a W,
    start_gen: u64,
    end_gen: u64,
    published: bool,
    phantom: PhantomData<T>,
}

impl<'a, W, T> Reservation<'a, W, T>
where
    W: AreaWriterTrait<T>
{
    /// Get the number of reserved slots
    pub fn len(&self) -> usize {
        (self.end_gen - self.start_gen) as usize
    }

    /// Check if the reservation is empty
    pub fn is_empty(&self) -> bool {
        self.start_gen >= self.end_gen
    }

    /// Get a mutable reference to the slot at the given index within the reservation.
    /// Returns `&mut MaybeUninit<T>`, since by definition the slot is allocated but uninitialized.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut MaybeUninit<T>> {
        if index >= self.len() {
            return None;
        }
        let generation = self.start_gen + index as u64;
        unsafe {
            let ptr = self.writer.get_slot_ptr(generation);
            // SAFETY:
            // 1. We have exclusive access to these slots via the reservation.
            // 2. T and MaybeUninit<T> have the same layout.
            let ptr = ptr as *mut MaybeUninit<T>;
            Some(&mut *ptr)
        }
    }

    /// Publish the reserved slots.
    /// Consumes the reservation.
    /// 
    /// # Safety
    /// This function is marked unsafe since it is the point at which the messages go out, and readers can access it with the assumption that all slots are initialized.
    /// If you have not initialized all slots in the reservation before calling this, readers will see uninitialized data and cause undefined behavior.
    pub unsafe fn publish(mut self) -> Result<(), Self> {
        self.published = true;
        match self.writer.publish_slots(self.start_gen, self.end_gen) {
            Ok(_) => Ok(()),
            Err(PublishError::CasFailed) => Err(self),
        }
    }

    /// Publish the reserved slots, spinning if necessary.
    /// Consumes the reservation.
    /// 
    /// # Safety
    /// This function is marked unsafe since it is the point at which the messages go out, and readers can access it with the assumption that all slots are initialized.
    /// If you have not initialized all slots in the reservation before calling this, readers will see uninitialized data and cause undefined behavior.
    pub unsafe fn publish_spin(self) {
        let mut reservation = self;
        while let Err(returned) = unsafe { reservation.publish() } {
            reservation = returned;
            core::hint::spin_loop();
        }
    }

    /// Split the reservation into two at the given index.
    /// The original reservation is consumed (and marked published to avoid panic).
    /// Returns (left, right) where left is [start, start + index) and right is [start + index, end).
    ///
    /// # Panics
    /// Panics if index > self.len()
    pub fn split_at_mut(mut self, index: usize) -> (Reservation<'a, W, T>, Reservation<'a, W, T>) {
        assert!(index <= self.len(), "index out of bounds");

        // Mark self as published so it doesn't panic on drop
        self.published = true;

        let split_gen = self.start_gen + index as u64;

        let left = Reservation {
            writer: self.writer,
            start_gen: self.start_gen,
            end_gen: split_gen,
            published: false,
            phantom: PhantomData,
        };

        let right = Reservation {
            writer: self.writer,
            start_gen: split_gen,
            end_gen: self.end_gen,
            published: false,
            phantom: PhantomData,
        };

        (left, right)
    }
}

impl<'a, W, T> Drop for Reservation<'a, W, T>
where
    W: AreaWriterTrait<T>
{
    fn drop(&mut self) {
        if !self.published {
            // We can't easily "unreserve" slots in this design without breaking the ring buffer continuity
            // or introducing holes, so we must panic to alert the developer.
            panic!(
                "Reservation dropped without publishing! Slots {}..{} are leaked/blocked.",
                self.start_gen, self.end_gen
            );
        }
    }
}

/// A reader handle for an Area
pub struct AreaReader<T> {
    inner: NonNull<AreaInner<T>>,
    reader_id: u64,
}

unsafe impl<T: Send> Send for AreaReader<T> {}
unsafe impl<T: Send> Sync for AreaReader<T> {}

impl<T> AreaReader<T> {
    /// Create a new reader handle for the same area
    pub fn create_reader(&self) -> Result<Self, RegisterError> {
        unsafe {
            let inner = self.inner.as_ref();
            let reader_id = inner.register_reader()?;
            Ok(AreaReader {
                inner: self.inner,
                reader_id,
            })
        }
    }

    /// Get the current generation number for this reader
    pub fn get_gen(&self) -> u64 {
        unsafe {
            self.inner
                .as_ref()
                .get_reader_gen(self.reader_id)
                .expect("Reader not found! This should not happen.")
        }
    }

    /// Load the current published read generation
    pub fn load_read_gen(&self) -> u64 {
        unsafe { self.inner.as_ref().load_read_gen() }
    }

    /// Advance this reader's generation by n messages
    fn advance(&mut self, n: u64) -> Result<(), SetGenError> {
        let current = self.get_gen();
        let new_generation = gen_without_suspended_bit(current) + n;
        unsafe {
            self.inner
                .as_ref()
                .set_reader_gen(self.reader_id, new_generation)
        }
    }

    /// Get a const pointer to the slot at the given generation number.
    ///
    /// # Safety
    /// - generation must be >= your reader's current generation
    /// - generation must be < the current read_gen (i.e., must be published)
    /// - You must not access the slot after advancing past it
    unsafe fn get_slot_ptr(&self, generation: u64) -> *const T {
        unsafe { self.inner.as_ref().get_slot_ptr_const(generation) }
    }

    /// Mark this reader as suspended
    pub fn suspend(&mut self) -> Result<(), SuspendError> {
        unsafe { self.inner.as_ref().suspend_reader(self.reader_id) }
    }

    /// Try to resume from suspended state at the current valid generation
    pub fn resume(&mut self) -> Result<bool, ResumeError> {
        unsafe { self.inner.as_ref().resume_reader(self.reader_id) }
    }

    /// Try to resume from suspended state at the given entry_generation
    pub fn resume_at(&mut self, entry_generation: u64) -> Result<bool, ResumeError> {
        unsafe {
            self.inner
                .as_ref()
                .resume_reader_at(self.reader_id, entry_generation)
        }
    }

    /// Try to cleanup old slots by advancing last_valid_gen
    #[inline]
    pub fn try_cleanup_old_slots(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.inner.as_ref().try_cleanup_old_slots() }
    }
}

impl<T> Drop for AreaReader<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ref();

            // Step 1: Increment reader_keep_alloc_tickets
            let keep_alloc = inner.reader_keep_alloc_tickets.as_ref();
            keep_alloc.fetch_add(1, Ordering::AcqRel);

            // Step 2: Unregister ourselves from the map
            //TODO: Check this.
            let _ = inner.unregister_reader(self.reader_id);

            // Step 3: Check if there are any other readers
            let has_readers = inner.has_readers();

            // Step 4-5: Logic based on whether we found any other readers
            let attempting_free = !has_readers;

            if attempting_free {
                // Step 6: Increment reader_stage_tickets
                let stage_tickets = inner.reader_stage_tickets.as_ref();
                stage_tickets.fetch_add(1, Ordering::AcqRel);
            }

            // Step 7: Decrement reader_keep_alloc_tickets
            keep_alloc.fetch_sub(1, Ordering::AcqRel);

            // Step 8: If not attempting to free, exit here
            if !attempting_free {
                return;
            }

            // Step 9: We're attempting to free - wait for reader_keep_alloc_tickets to hit 0
            while keep_alloc.load(Ordering::Acquire) != 0 {
                core::hint::spin_loop();
            }

            // Now decrement reader_stage_tickets
            let stage_tickets = inner.reader_stage_tickets.as_ref();
            let prev_stage = stage_tickets.fetch_sub(1, Ordering::AcqRel);

            // If we got it to 0, we're the elected representative
            if prev_stage == 1 {
                // Decrement destroy_stages
                let destroy_stages = inner.destroy_stages.as_ref();
                let prev_stages = destroy_stages.fetch_sub(1, Ordering::AcqRel);

                // If we got destroy_stages to 0, we're responsible for final cleanup
                if prev_stages == 1 {
                    // Drop all remaining items in the buffer
                    let _ = inner.try_cleanup_old_slots();

                    // Free the AreaInner
                    let inner_box = Box::from_raw(self.inner.as_ptr());

                    // Free the leaked atomics
                    let _ = Box::from_raw(inner_box.destroy_stages.as_ptr());
                    let _ = Box::from_raw(inner_box.reader_keep_alloc_tickets.as_ptr());
                    let _ = Box::from_raw(inner_box.reader_stage_tickets.as_ptr());

                    // inner_box is dropped here, freeing AreaInner
                }
            }
        }
    }
}

/// A slice-like view of readable messages that auto-advances on drop
pub struct ReadSlice<'a, T> {
    reader: &'a mut AreaReader<T>,
    start_gen: u64,
    end_gen: u64,
    armed: bool,
}

impl<'a, T> ReadSlice<'a, T> {
    /// Create a new ReadSlice that is disarmed by default (won't auto-advance on drop).
    /// Caller must manually advance.
    ///
    /// # Safety
    /// - start_gen and end_gen must be valid generation numbers
    pub unsafe fn new_disarmed(
        reader: &'a mut AreaReader<T>,
        start_gen: u64,
        end_gen: u64,
    ) -> Self {
        Self {
            reader,
            start_gen,
            end_gen,
            armed: false,
        }
    }

    /// Disarm this slice so it won't auto-advance on drop.
    /// The caller is responsible for manually advancing the reader if needed.
    ///
    /// # Safety
    /// - The caller must ensure the reader is advanced appropriately
    pub unsafe fn disarm(&mut self) {
        self.armed = false;
    }

    /// Turns this slice into its components.
    ///
    /// # Safety
    /// - The caller must ensure the reader is advanced appropriately
    pub unsafe fn into_raw_parts(self) -> (&'a mut AreaReader<T>, u64, u64, bool) {
        let manually_dropped_s = core::mem::ManuallyDrop::new(self);

        let reader;
        let start_gen;
        let end_gen;
        let armed;
        unsafe {
            reader = core::ptr::read(&raw const manually_dropped_s.reader);
            start_gen = core::ptr::read(&raw const manually_dropped_s.start_gen);
            end_gen = core::ptr::read(&raw const manually_dropped_s.end_gen);
            armed = core::ptr::read(&raw const manually_dropped_s.armed);
        }

        (reader, start_gen, end_gen, armed)
    }

    /// Get the number of messages in this slice
    pub fn len(&self) -> usize {
        (self.end_gen - self.start_gen) as usize
    }

    /// Check if the slice is empty
    pub fn is_empty(&self) -> bool {
        self.start_gen >= self.end_gen
    }

    /// Get a reference to the message at the given index
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        let generation = self.start_gen + index as u64;
        unsafe {
            let ptr = self.reader.get_slot_ptr(generation);
            Some(&*ptr)
        }
    }

    /// Iterate over all messages in the slice
    pub fn iter(&self) -> ReadSliceIter<'_, T> {
        ReadSliceIter {
            slice: self,
            current: 0,
        }
    }

    /// Get the messages as one or two slices (if wrapping around the ring buffer)
    pub fn as_slices(&self) -> (&[T], &[T]) {
        if self.is_empty() {
            return (&[], &[]);
        }

        let cap = unsafe { self.reader.inner.as_ref().buffer_capacity };
        let start_idx = (self.start_gen as usize) % cap;

        // Calculate the length of the first segment
        let len = (self.end_gen - self.start_gen) as usize;

        unsafe {
            let buffer_ptr = self.reader.inner.as_ref().buffer.as_ptr() as *const T;

            if start_idx + len <= cap {
                // Contiguous
                let slice = core::slice::from_raw_parts(buffer_ptr.add(start_idx), len);
                (slice, &[])
            } else {
                // Wraps around
                let first_len = cap - start_idx;
                let second_len = len - first_len;
                let first = core::slice::from_raw_parts(buffer_ptr.add(start_idx), first_len);
                let second = core::slice::from_raw_parts(buffer_ptr, second_len);
                (first, second)
            }
        }
    }

    /// Try to cleanup old slots by advancing last_valid_gen
    #[inline]
    pub fn try_cleanup_old_slots(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.reader.inner.as_ref().try_cleanup_old_slots() }
    }
}

impl<'a, T> Drop for ReadSlice<'a, T> {
    fn drop(&mut self) {
        if !self.armed {
            // Slice was disarmed, don't auto-advance
            return;
        }

        // Check that the reader hasn't been manually advanced while we held the slice
        let current_gen = gen_without_suspended_bit(self.reader.get_gen());
        if current_gen != self.start_gen {
            panic!(
                "ReadSlice dropped with reader generation mismatch: expected {}, found {}. \
                 Reader was manually advanced while ReadSlice was alive.",
                self.start_gen, current_gen
            );
        }

        // Automatically advance the reader past all messages in this slice
        let n = self.end_gen - self.start_gen;
        if n > 0 {
            let _ = self.reader.advance(n);
        }
    }
}

impl<T> AreaReader<T> {
    /// Get a slice of all currently available messages.
    /// When the slice is dropped, the reader automatically advances past these messages.
    pub fn read(&mut self) -> ReadSlice<'_, T> {
        let start_generation = gen_without_suspended_bit(self.get_gen());
        let end_generation = self.load_read_gen();
        ReadSlice {
            reader: self,
            start_gen: start_generation,
            end_gen: end_generation,
            armed: true,
        }
    }

    /// Get a slice of all currently available messages.
    /// When the slice is dropped, the reader automatically advances past these messages.
    /// 
    /// If no writers are still in the area, returns a [`ReadError`].
    /// 
    /// # Behavior to note
    /// 
    /// This method only checks for **AreaWriters,** not other AreaReaders. If you are primarily using the readers-are-also-writers pattern, use [`AreaReader::read`] instead and implement your own check for exit conditions.
    pub fn read_with_check(&mut self) -> Result<ReadSlice<'_, T>, ReadError> {
        let (returned_self, start_gen, _, _) = {
            let result = self.read();

            if result.len() != 0 {
                return Ok(result);
            }

            unsafe {
                result.into_raw_parts()
            }
        };

        if returned_self.get_writers_count() == 0 {
            // There is an ABA issue here that can make us miss messages if we have new messages that are posted in-between our read and this check.
            // To fix this, we do a read again.
            let result = returned_self.read();

            if result.len() != 0 {
                return Ok(result);
            }

            return Err(ReadError);
        }

        unsafe {
            return Ok(ReadSlice::new_disarmed(returned_self, start_gen, start_gen));
        }
    }
}

/// Iterator over messages in a ReadSlice
pub struct ReadSliceIter<'a, T> {
    slice: &'a ReadSlice<'a, T>,
    current: usize,
}

impl<'a, T> Iterator for ReadSliceIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.slice.get(self.current)?;
        self.current += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.slice.len() - self.current;
        (remaining, Some(remaining))
    }
}

impl<'a, T> ExactSizeIterator for ReadSliceIter<'a, T> {}

/// Builder for creating an Area
pub struct AreaBuilder {
    buffer_capacity: usize,
    reader_capacity: usize,
}

impl AreaBuilder {
    /// Create a new builder with default settings
    pub fn new() -> Self {
        Self {
            buffer_capacity: 1024,
            reader_capacity: 128,
        }
    }

    /// Set the ring buffer capacity (must be > 0)
    pub fn buffer_capacity(mut self, capacity: usize) -> Self {
        self.buffer_capacity = capacity;
        self
    }

    /// Set the maximum number of readers (must be a power of two)
    pub fn reader_capacity(mut self, capacity: usize) -> Self {
        self.reader_capacity = capacity;
        self
    }

    /// Build the Area, returning a writer and a reader handle
    pub fn build<T>(self) -> (AreaWriter<T>, AreaReader<T>) {
        area(self.buffer_capacity, self.reader_capacity)
    }
}

impl Default for AreaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a new area with the given buffer and reader capacity.
/// Returns a writer and a reader handle.
/// Panics if initial reader registration fails (e.g. capacity reached).
pub fn area<T>(buffer_capacity: usize, reader_capacity: usize) -> (AreaWriter<T>, AreaReader<T>) {
    let inner = AreaInner::new(buffer_capacity, reader_capacity);

    unsafe {
        let inner_ref = inner.as_ref();
        // Set writers count to 1 (the returned writer)
        inner_ref.writers_count.store(1, Ordering::Release);

        // Register the initial reader
        let reader_id = match inner_ref.register_reader() {
            Ok(id) => id,
            Err(RegisterError::ReaderCapacityReached) => {
                panic!("Initial reader registration failed: capacity reached")
            }
        };

        let writer = AreaWriter { inner };
        let reader = AreaReader { inner, reader_id };

        (writer, reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_writer_reserve_and_publish() {
        let (writer, reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Initial state
        assert_eq!(reader.get_gen(), 0);
        // Reserve 4 slots
        let (start, end) = writer.try_reserve_slots(4).expect("Failed to reserve");
        assert_eq!(start, 0);
        assert_eq!(end, 4);

        // Write to the slots
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation * 10);
            }
        }

        // Publish the slots
        writer.publish_slots(start, end).expect("Failed to publish");

        // Verify read_gen moved forward
        unsafe {
            let read_gen = writer.inner.as_ref().load_read_gen();
            assert_eq!(read_gen, 4);
        }
    }

    #[test]
    fn test_reservation_write_and_publish() {
        let (writer, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Reserve 4 slots
        let mut reservation = writer.reserve(4).unwrap();
        
        // Write to reservation
        for i in 0..4 {
            reservation.get_mut(i).unwrap().write(100 + i as u64);
        }
        
        unsafe { reservation.publish() }.unwrap();
    }

    #[test]
    fn test_reservation_split() {
        let (writer, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Reserve 4 slots
        let reservation = writer.reserve(4).unwrap();
        assert_eq!(reservation.len(), 4);

        // Split at 2
        let (mut left, mut right) = reservation.split_at_mut(2);

        assert_eq!(left.len(), 2);
        assert_eq!(right.len(), 2);

        // Write to both
        left.get_mut(0).unwrap().write(100);
        left.get_mut(1).unwrap().write(101);
        right.get_mut(0).unwrap().write(102);
        right.get_mut(1).unwrap().write(103);

        // Verify correct ordering works.
        match unsafe { right.publish() } {
            Ok(_) => panic!("Wrong order works; there's a bug"),
            Err(right) => {
                assert!(unsafe { left.publish() }.is_ok());
                assert!(unsafe { right.publish() }.is_ok());
            },
        }
    }

    #[test]
    fn test_reservation_raii() {
        let (writer, reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Use reservation
        let mut reservation = writer.reserve(4).expect("Failed to reserve");
        assert_eq!(reservation.len(), 4);

        // Write to slots
        for i in 0..4 {
            reservation.get_mut(i).unwrap().write((i as u64) * 100);
        }

        // Publish
        unsafe { reservation.publish_spin() };

        // Verify
        let slice = reader.load_read_gen();
        assert_eq!(slice, 4);
    }

    #[test]
    fn test_reservation_inplace_init() {
        #[derive(Debug)]
        struct ComplexStruct {
            a: u64,
            b: Vec<u8>,
        }

        // Track drops to ensure we don't double-drop uninitialized memory
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        impl Drop for ComplexStruct {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let (writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<ComplexStruct>();

        let mut reservation = writer.reserve(1).unwrap();
        let slot = reservation.get_mut(0).unwrap();

        // In-place initialization using raw pointers
        let ptr = slot.as_mut_ptr();
        unsafe {
            (&raw mut (*ptr).a).write(42);
            (&raw mut (*ptr).b).write(alloc::vec![1, 2, 3]);
        }

        unsafe { reservation.publish() }.unwrap();

        // Verify read
        let slice = reader.read_with_check().expect("There should be writers!");
        let (s1, _) = slice.as_slices();
        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].a, 42);
        assert_eq!(s1[0].b, alloc::vec![1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "Reservation dropped without publishing")]
    fn test_reservation_panic_on_drop() {
        let (writer, _reader) = area::<u64>(16, 8);
        let _reservation = writer.reserve(4).expect("Failed to reserve");
        // Drop without publish -> panic
    }

    #[test]
    fn test_reader_registration_and_reading() {
        let (writer, reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Write some messages
        let (start, end) = writer.try_reserve_slots(3).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation + 100);
            }
        }
        writer.publish_slots(start, end).expect("Failed to publish");

        // Reader should be able to read
        let reader_gen = reader.get_gen();
        assert_eq!(reader_gen, 0);

        let read_gen = reader.load_read_gen();
        assert_eq!(read_gen, 3);

        // Read the messages
        unsafe {
            for generation in reader_gen..read_gen {
                let ptr = reader.get_slot_ptr(generation);
                let value = ptr.read();
                assert_eq!(value, generation + 100);
            }
        }
    }

    #[test]
    fn test_register_capacity_reached() {
        let (_writer, reader1) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(2) // Small capacity
            .build::<u64>();

        // Capacity is 2. reader1 takes one slot.

        // Create another reader
        let _reader2 = reader1.create_reader().expect("Should succeed");

        // Now capacity should be full (2 readers)
        // Trying to create a 3rd reader should fail
        let result = reader1.create_reader();
        assert_eq!(result.err(), Some(RegisterError::ReaderCapacityReached));
    }

    #[test]
    fn test_create_writer() {
        let (writer1, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();
        let writer2 = writer1.create_writer();

        // Both writers should work
        let (start1, end1) = writer1.try_reserve_slots(2).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 2);

        let (start2, end2) = writer2.try_reserve_slots(2).expect("Failed to reserve");
        assert_eq!(start2, 2);
        assert_eq!(end2, 4);
    }

    #[test]
    fn test_create_reader() {
        let (writer, reader1) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();
        let reader2 = reader1.create_reader().expect("Failed to create reader");

        // Different reader IDs
        assert_ne!(reader1.reader_id, reader2.reader_id);

        // Write some messages
        let (start, end) = writer.try_reserve_slots(3).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation * 2);
            }
        }
        writer.publish_slots(start, end).expect("Failed to publish");

        // Both readers can read
        let read_gen = reader1.load_read_gen();
        assert_eq!(read_gen, 3);
        assert_eq!(reader2.load_read_gen(), 3);
    }

    #[test]
    fn test_reserve_overflow() {
        let (writer, _reader) = AreaBuilder::new()
            .buffer_capacity(4)
            .reader_capacity(8)
            .build::<u64>();

        // Fill the buffer
        let (start1, end1) = writer.try_reserve_slots(4).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 4);

        // Try to reserve more - should fail
        let result = writer.try_reserve_slots(1);
        assert_eq!(result, Err(ReserveError::NoSpace));
    }

    #[test]
    fn test_best_effort_reserve() {
        let (writer, _reader) = AreaBuilder::new()
            .buffer_capacity(4)
            .reader_capacity(8)
            .build::<u64>();

        // Reserve 2 slots normally
        let (start1, end1) = writer.try_reserve_slots(2).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 2);

        // Try to reserve 10 slots with best effort - should get only 2
        let (start2, end2, actual) = writer
            .try_reserve_slots_best_effort(10)
            .expect("Failed to reserve");
        assert_eq!(start2, 2);
        assert_eq!(end2, 4);
        assert_eq!(actual, 2);

        // Try to reserve again, should get 0
        let (start3, end3, actual2) = writer
            .try_reserve_slots_best_effort(10)
            .expect("Failed to reserve");
        assert_eq!(start3, 0);
        assert_eq!(end3, 0);
        assert_eq!(actual2, 0);

        // Try to reserve more normally, should fail
        assert_eq!(writer.try_reserve_slots(2), Err(ReserveError::NoSpace));
    }

    #[test]
    fn test_suspend_and_resume() {
        let (_writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Suspend the reader
        assert!(reader.suspend().is_ok());

        let generation = reader.get_gen();
        assert!(is_suspended(generation));

        // Resume the reader
        reader.resume().expect("Failed to resume");

        let generation = reader.get_gen();
        assert!(!is_suspended(generation));
    }

    #[test]
    fn test_resume_at() {
        let (_writer, mut reader) = area::<u64>(16, 8);

        // Suspend
        assert!(reader.suspend().is_ok());

        // Resume at specific generation
        reader.resume_at(0).expect("Failed to resume");
        assert_eq!(reader.get_gen(), 0);

        // Suspend again
        assert!(reader.suspend().is_ok());

        // Try to resume again
        reader.resume_at(0).expect("Failed to resume");
    }

    #[test]
    fn test_cleanup_simple() {
        let (writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Write some messages
        let (start, end) = writer.try_reserve_slots(5).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation);
            }
        }
        writer.publish_slots(start, end).expect("Failed to publish");

        // Reader advances past the messages
        reader.advance(5).expect("Failed to advance");

        // Cleanup should work now
        let result = writer.try_cleanup_old_slots();
        assert!(result.is_ok());

        let cleanup_result = result.unwrap();
        assert_eq!(cleanup_result.old_last_valid, 0);
        assert_eq!(cleanup_result.new_last_valid, 5);
        assert_eq!(cleanup_result.slots_cleaned, 5);
    }

    #[test]
    fn test_unregister_unblocks_writer() {
        let (writer, reader) = AreaBuilder::new()
            .buffer_capacity(4)
            .reader_capacity(8)
            .build::<u64>();

        // Reader stays at 0

        // Fill the buffer
        let (start, end) = writer.try_reserve_slots(4).expect("Failed to reserve");
        writer.publish_slots(start, end).expect("Failed to publish");

        // Writer should be blocked now
        assert_eq!(writer.try_reserve_slots(1), Err(ReserveError::NoSpace));

        // Unregister the reader (dropping it)
        drop(reader);

        // Trigger cleanup.
        let cleanup_result = writer
            .try_cleanup_old_slots()
            .expect("Cleanup should succeed");
        assert_eq!(cleanup_result.new_last_valid, 4);

        // Now writer can write
        let (start, end) = writer.try_reserve_slots(4).expect("Failed to reserve");
        assert_eq!(start, 4);
        assert_eq!(end, 8);
    }

    #[test]
    fn test_writer_drop_decrements_count() {
        let (writer1, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        unsafe {
            let count_before = writer1.inner.as_ref().writers_count.load(Ordering::Acquire);
            assert_eq!(count_before, 1);
        }

        {
            let writer2 = writer1.create_writer();
            unsafe {
                let count_with_two = writer1.inner.as_ref().writers_count.load(Ordering::Acquire);
                assert_eq!(count_with_two, 2);
            }
            drop(writer2);
        }

        unsafe {
            let count_after = writer1.inner.as_ref().writers_count.load(Ordering::Acquire);
            assert_eq!(count_after, 1);
        }
    }

    #[test]
    fn test_suspend_idempotent() {
        let (_writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        assert!(reader.suspend().is_ok());
        assert!(is_suspended(reader.get_gen()));

        // Suspend again should error
        match reader.suspend() {
            Ok(_) => panic!("Suspending again should error"),
            Err(SuspendError::ReaderNotFound) => panic!("Reader unregistered!"),
            Err(SuspendError::AlreadySuspended) => {},
        }
        assert!(is_suspended(reader.get_gen()));
    }

    #[test]
    fn test_advance_while_suspended_fails() {
        let (_writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        assert!(reader.suspend().is_ok());

        // Try to advance while suspended
        let result = reader.advance(1);
        assert_eq!(result, Err(SetGenError::ReaderSuspended));
    }

    #[test]
    fn test_publish_cas_failure() {
        let (writer1, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();
        let writer2 = writer1.create_writer();

        // Writer1 reserves slots 0-2
        let (start1, end1) = writer1.try_reserve_slots(2).expect("Failed to reserve");
        unsafe {
            for generation in start1..end1 {
                let ptr = writer1.get_slot_ptr(generation);
                ptr.write(generation);
            }
        }

        // Writer2 reserves slots 2-4
        let (start2, end2) = writer2.try_reserve_slots(2).expect("Failed to reserve");
        unsafe {
            for generation in start2..end2 {
                let ptr = writer2.get_slot_ptr(generation);
                ptr.write(generation);
            }
        }

        // If writer2 tries to publish first, it should fail
        let result = writer2.publish_slots(start2, end2);
        assert_eq!(result, Err(PublishError::CasFailed));

        // Writer1 publishes successfully
        writer1
            .publish_slots(start1, end1)
            .expect("Failed to publish");

        // Writer2 publishes successfully
        writer2
            .publish_slots(start2, end2)
            .expect("Failed to publish");

        // Now if writer1 tries to publish again with stale range, it should fail
        let result = writer1.publish_slots(start1, end1);
        assert_eq!(result, Err(PublishError::CasFailed));
    }

    #[test]
    fn test_drop_cleanup() {
        // This test verifies that dropping all handles properly cleans up
        // We can't directly observe the cleanup, but we can ensure it doesn't panic
        {
            let (writer, reader) = AreaBuilder::new()
                .buffer_capacity(16)
                .reader_capacity(8)
                .build::<u64>();
            let _writer2 = writer.create_writer();
            let _reader2 = reader.create_reader().expect("Failed to create reader");

            // Use the handles a bit
            let (start, end) = writer.try_reserve_slots(2).expect("Failed to reserve");
            unsafe {
                for generation in start..end {
                    let ptr = writer.get_slot_ptr(generation);
                    ptr.write(generation);
                }
            }
            writer.publish_slots(start, end).expect("Failed to publish");

            // All handles will drop here
        }
        // If we get here without panicking, the cleanup worked
    }

    #[test]
    fn test_read_slice() {
        let (writer, mut reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Write some messages
        let (start, end) = writer.try_reserve_slots(5).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation * 100);
            }
        }
        writer.publish_slots(start, end).expect("Failed to publish");

        // Read using slice API
        {
            let slice = reader.read_with_check().expect("There should be writers!");
            assert_eq!(slice.len(), 5);
            assert!(!slice.is_empty());

            // Test get()
            assert_eq!(*slice.get(0).unwrap(), 0);
            assert_eq!(*slice.get(1).unwrap(), 100);
            assert_eq!(*slice.get(4).unwrap(), 400);
            assert!(slice.get(5).is_none());

            // Test iterator
            let values: Vec<u64> = slice.iter().copied().collect();
            assert_eq!(values, alloc::vec![0, 100, 200, 300, 400]);

            // Test as_slices
            let (s1, s2) = slice.as_slices();
            assert_eq!(s1.len() + s2.len(), 5);
            // Since capacity is 16 and we start at 0, it should be contiguous
            assert_eq!(s1.len(), 5);
            assert_eq!(s2.len(), 0);
            assert_eq!(s1, &[0, 100, 200, 300, 400]);

            // Slice drops here, auto-advancing reader
        }

        // Reader should have advanced
        let reader_gen = reader.get_gen();
        assert_eq!(reader_gen, 5);

        // New messages
        let (start, end) = writer.try_reserve_slots(2).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation * 10);
            }
        }
        writer.publish_slots(start, end).expect("Failed to publish");

        // Read again
        {
            let slice = reader.read_with_check().expect("There should be writers!");
            assert_eq!(slice.len(), 2);
            assert_eq!(*slice.get(0).unwrap(), 50);
            assert_eq!(*slice.get(1).unwrap(), 60);
        }

        assert_eq!(reader.get_gen(), 7);
    }

    #[test]
    fn test_register_reader_respects_cleanup() {
        let (writer, mut reader1) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(8)
            .build::<u64>();

        // Advance everything to generation 10
        let (start, end) = writer.try_reserve_slots(10).unwrap();
        unsafe {
            for i in start..end {
                writer.get_slot_ptr(i).write(i);
            }
        }
        writer.publish_slots(start, end).unwrap();
        reader1.advance(10).unwrap();

        // Cleanup should advance last_valid to 10
        // And it should drag all empty slots to 10
        let result = writer.try_cleanup_old_slots().unwrap();
        assert_eq!(result.new_last_valid, 10);

        // Now register a new reader
        // It should pick up a slot that was dragged to 10
        // So it should start at 10
        let reader2 = reader1.create_reader().expect("Failed to create reader");
        assert_eq!(reader2.get_gen(), 10);
    }

    #[test]
    #[should_panic(expected = "reader_capacity must be a power of two")]
    fn test_area_creation_panics_on_invalid_capacity() {
        let (_writer, _reader) = AreaBuilder::new()
            .buffer_capacity(16)
            .reader_capacity(0)
            .build::<u64>();
    }
}
