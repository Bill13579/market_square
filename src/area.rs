use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(debug_assertions)]
pub(crate) const SPIN_LIMIT: usize = usize::MAX;

use crate::{arithmetics::{AtomicType, MSB, NumericType, gen_add_msb_masked, gen_dist_msb_masked, gen_gt_msb_masked, gen_gte_msb_masked, gen_lt_msb_masked, gen_lte_msb_masked}, map::Slot, storage::{FixedStorage, FixedStorageMultiple}};

#[cfg(any(feature = "std", feature = "alloc"))]
use crate::storage::{BoxedSliceStorage, BoxedStorage};

use crate::cache_padded::CachePadded;
#[cfg(feature = "std")]
use rand::Rng;

use crate::map::{self, IMMEDIATE, SimpleLPHashMap, ZERO_OFFSET};

// MSB is reserved for "suspended" status on reader generation numbers
pub const SUSPENDED_BIT: NumericType = MSB;

/// Strip the suspended bit from a generation number
#[inline]
pub fn gen_without_suspended_bit(generation: NumericType) -> NumericType {
    generation & !SUSPENDED_BIT
}

/// Check if a generation number has the suspended bit set
#[inline]
pub fn is_suspended(generation: NumericType) -> bool {
    (generation & SUSPENDED_BIT) != 0
}

/// Errors that can occur when reserving slots
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// No space available in the ring buffer (capacity check failed)
    NoSpace,
    /// Failed to grab slots (race condition, retry might succeed)
    FailedGrab,
}

/// Errors that can occur when publishing slots
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    /// The range doesn't match the current read_gen (someone else published in between, or someone else with earlier generations have not yet published)
    CasFailed,
    /// Publishing out of order (the start_generation of the range to be published didn't match the current read_gen)
    /// 
    /// Only occurs in Exclusive mode (when `E = Exclusive`), since in the CAS mode (when `E = ()`) the start range check is implicit in the CAS operation.
    /// 
    /// read_gen should be at start_generation when publishing with range [start_generation, end_generation), since otherwise readers might observe unpublished and uninitialized slots, leading to undefined behavior. If this assertion fails, some code is publishing out-of-order. Publishing must be strictly in the order of the generations returned by try_reserve_slots, in sequential order.
    OutOfOrderPublishAttempt,
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
    pub old_last_valid: NumericType,
    /// The new last_valid_gen after cleanup
    pub new_last_valid: NumericType,
    /// Number of slots cleaned
    pub slots_cleaned: NumericType,
}

/// The shared inner state of an Area
pub struct AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Next generation number for writing (monotonically increasing)
    write_gen: CachePadded<AtomicType>,

    /// Current generation number published for reading (monotonically increasing)
    read_gen: CachePadded<AtomicType>,

    /// Last generation number that is still valid (for cleanup)
    last_valid_gen: CachePadded<AtomicType>,

    /// Generation numbers that are now freed and can be reused
    free_gen: CachePadded<AtomicType>,

    /// Number of active writers
    writers_count: CachePadded<AtomicUsize>,

    /// Per-reader generation numbers (with MSB as suspended flag)
    /// Each value is CachePadded<AtomicType> for cache-line isolation
    reader_gens: SimpleLPHashMap<CachePadded<AtomicType>, SReaderGens>,

    /// Ring buffer capacity
    buffer_capacity: usize,

    /// Ring buffer storage
    buffer: SBuf,

    /// Destruction stages counter (leaked, separately allocated)
    /// Starts at 2 (1 for writers, 1 for readers collectively)
    destroy_stages: SAtomicUsizeCounter,

    /// Reader keep-alloc tickets (leaked, separately allocated)
    /// Used to gate non-competing reader destructors
    reader_keep_alloc_tickets: SAtomicUsizeCounter,

    /// Reader stage tickets (leaked, separately allocated)
    /// Count of readers competing to decrement destroy_stages
    reader_stage_tickets: SAtomicUsizeCounter,

    phantom: PhantomData<T>,
}

unsafe impl<T, SReaderGens, SBuf, SAtomicUsizeCounter> Send for AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    T: Send,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}
unsafe impl<T, SReaderGens, SBuf, SAtomicUsizeCounter> Sync for AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    T: Send,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}

#[cfg(any(feature = "std", feature = "alloc"))]
impl<T> AreaInner<
    T,
    BoxedSliceStorage<Slot<CachePadded<AtomicType>>>,
    BoxedSliceStorage<UnsafeCell<MaybeUninit<T>>>,
    BoxedStorage<AtomicUsize>
> {
    /// Create a new AreaInner with the given buffer capacity and reader capacity in newly allocated Boxed storage.
    fn new(buffer_capacity: usize, reader_capacity: usize) -> BoxedStorage<Self> {
        assert!(buffer_capacity > 0, "buffer_capacity must be > 0");
        assert!(
            reader_capacity.is_power_of_two(),
            "reader_capacity must be a power of two"
        );
        assert!(reader_capacity > 0, "reader_capacity must be > 0");

        let buffer = BoxedSliceStorage::with_capacity_and_init(buffer_capacity, || UnsafeCell::new(MaybeUninit::uninit()));

        // Allocate and leak the destruction coordination atomics

        let destroy_stages = BoxedStorage::new(AtomicUsize::new(2));
        let reader_keep_alloc_tickets = BoxedStorage::new(AtomicUsize::new(0));
        let reader_stage_tickets = BoxedStorage::new(AtomicUsize::new(0));

        let inner = Self {
            write_gen: CachePadded::new(AtomicType::new(0)),
            read_gen: CachePadded::new(AtomicType::new(0)),
            last_valid_gen: CachePadded::new(AtomicType::new(0)),
            free_gen: CachePadded::new(AtomicType::new(0)),
            writers_count: CachePadded::new(AtomicUsize::new(0)),
            reader_gens: SimpleLPHashMap::with_capacity_and_init(reader_capacity, || {
                CachePadded::new(AtomicType::new(SUSPENDED_BIT))
            }),
            buffer_capacity,
            buffer,
            destroy_stages,
            reader_keep_alloc_tickets,
            reader_stage_tickets,
            phantom: PhantomData,
        };

        // Leak the AreaInner and return a BoxedStorage pointer
        BoxedStorage::new(inner)
    }
}

/// A marker type for specifying that CAS can be skipped for a writer operation because the caller is the only active writer.
#[allow(dead_code)]
pub struct Exclusive(u8);

impl<T, SReaderGens, SBuf, SAtomicUsizeCounter> AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>
where
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Initialize a new [`AreaInner`] in existing storage.
    /// 
    /// # Notes
    /// - `reader_gens` must have all values initialized not to zero but to `SUSPENDED_BIT`.
    pub fn init(
        area_inner: *mut Self,
        reader_gens: SimpleLPHashMap<CachePadded<AtomicType>, SReaderGens>,
        buffer: SBuf,
        destroy_stages: SAtomicUsizeCounter,
        reader_keep_alloc_tickets: SAtomicUsizeCounter,
        reader_stage_tickets: SAtomicUsizeCounter,
    ) {
        unsafe {
            (&raw mut (*area_inner).write_gen).write(CachePadded::new(AtomicType::new(0)));
            (&raw mut (*area_inner).read_gen).write(CachePadded::new(AtomicType::new(0)));
            (&raw mut (*area_inner).last_valid_gen).write(CachePadded::new(AtomicType::new(0)));
            (&raw mut (*area_inner).free_gen).write(CachePadded::new(AtomicType::new(0)));
            (&raw mut (*area_inner).writers_count).write(CachePadded::new(AtomicUsize::new(0)));
            (&raw mut (*area_inner).reader_gens).write(reader_gens);
            (&raw mut (*area_inner).buffer_capacity).write(buffer.capacity());
            (&raw mut (*area_inner).buffer).write(buffer);
            (&raw mut (*area_inner).destroy_stages).write(destroy_stages);
            (&raw mut (*area_inner).reader_keep_alloc_tickets).write(reader_keep_alloc_tickets);
            (&raw mut (*area_inner).reader_stage_tickets).write(reader_stage_tickets);
            // (&raw mut (*area_inner).phantom).write(PhantomData);
        }
    }

    /// Get a mutable pointer to the slot at the given generation number
    #[inline]
    unsafe fn get_slot_ptr_mut(&self, generation: NumericType) -> *mut T {
        let index = (generation as usize) % self.buffer_capacity;
        unsafe { (*self.buffer.slice()[index].get()).as_mut_ptr() }
    }

    /// Get a const pointer to the slot at the given generation number
    #[inline]
    unsafe fn get_slot_ptr_const(&self, generation: NumericType) -> *const T {
        let index = (generation as usize) % self.buffer_capacity;
        unsafe { (*self.buffer.slice()[index].get()).as_ptr() }
    }

    /// Load the current write generation (non-inclusive)
    #[inline]
    fn load_write_gen(&self) -> NumericType {
        self.write_gen.load(Ordering::Acquire)
    }

    /// Load the current read generation (non-inclusive)
    #[inline]
    fn load_read_gen(&self) -> NumericType {
        self.read_gen.load(Ordering::Acquire)
    }

    /// Load the current last valid generation (inclusive)
    #[inline]
    fn load_last_valid_gen(&self) -> NumericType {
        self.last_valid_gen.load(Ordering::Acquire)
    }

    /// Load the current free generation (non-inclusive)
    #[inline]
    fn load_free_gen(&self) -> NumericType {
        self.free_gen.load(Ordering::Acquire)
    }

    /// Load the number of active writers
    #[inline]
    fn load_writers_count(&self) -> usize {
        self.writers_count.load(Ordering::Acquire)
    }

    /// Try to reserve n slots for writing, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// Returns (start_generation, end_generation) on success.
    /// end_generation is exclusive (so the range is [start_generation, end_generation)).
    fn try_reserve_slots<E>(&self, n: usize) -> Result<(NumericType, NumericType), ReserveError> {
        debug_assert!(n != 0, "must reserve at least one slot");

        // Load current state
        let current_free = self.load_free_gen();
        let current_write = self.load_write_gen(); //NOTE: Must come AFTER loading current_free to be conservative about free spots being available.
        debug_assert!(gen_lte_msb_masked(current_free, current_write), "free_gen should never exceed write_gen! this should never happen. {} {} {}", current_free, current_write, self.load_read_gen());
        let available = self.buffer_capacity as NumericType - (self.buffer_capacity as NumericType).min(gen_dist_msb_masked(current_write, current_free)); //NOTE: If current_write - current_free > buffer_capacity, that's because current_free hasn't caught up yet, since we never allow writers to lap readers by more than buffer_capacity. In that case, be conservative and report 0 available slots.

        // Check if there's enough capacity
        if (n as NumericType) > available {
            return Err(ReserveError::NoSpace);
        }

        if core::mem::size_of::<E>() == 0 { // CAS case
            // Try to CAS write_gen from current_write to current_write + n
            let expected_new_write = gen_add_msb_masked(current_write, n as NumericType);
            // Important Note: Here, it's theoretically possible now that we are wrapping that we somehow update write_gen to a value *equal* to free_gen.
            // Usually without overflow, this isn't possible since free_gen is always behind write_gen while write_gen monotonically increases.
            // However, if write_gen wraps, then it can become at least free_gen (it won't go beyond it on the first iteration since "available" is
            // always at most enough on the first iteration to advance write_gen to exactly free_gen)
            // If this happens though, the next time we try to reserve, we'll see free_gen == write_gen, and report every slot as available (because
            // gen_dist_msb_masked(current_write, current_free) == 0), which is bad.
            // However in practice for this to happen free_gen would have to be so behind in cleanup that the computer has probably already crashed already,
            // since this requires 1 billion messages (for AtomicU32 for instance, 2^30 messages) to be left uncleaned on the buffer while writers keep trying to reserve
            // more slots.
            match self.write_gen.compare_exchange(
                current_write,
                expected_new_write,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => Ok((current_write, expected_new_write)),
                Err(_) => Err(ReserveError::FailedGrab),
            }
        } else {
            // Update write_gen from current_write to current_write + n
            let new_write_gen = gen_add_msb_masked(current_write, n as NumericType);
            // See "Important Note" from try_reserve_slots above.
            self.write_gen.store(new_write_gen, Ordering::Release);
            
            Ok((current_write, new_write_gen))
        }
    }

    /// Try to reserve up to n slots, getting as many as possible, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// Returns (start_generation, end_generation, actual_count) or [`ReserveError::FailedGrab`] on race (only when `E = ()`).
    /// Never returns [`ReserveError::NoSpace`], since in the case that no slots are available it simply returns `Ok((0, 0, 0))`.
    fn try_reserve_slots_best_effort<E>(&self, n: usize) -> Result<(NumericType, NumericType, usize), ReserveError> {
        if n == 0 {
            return Ok((0, 0, 0));
        }

        // Load current state
        let current_free = self.load_free_gen();
        let current_write = self.load_write_gen(); //NOTE: Must come AFTER loading current_free to be conservative about free spots being available.
        debug_assert!(gen_lte_msb_masked(current_free, current_write), "free_gen should never exceed write_gen! this should never happen. {} {} {}", current_free, current_write, self.load_read_gen());
        let available = self.buffer_capacity as NumericType - (self.buffer_capacity as NumericType).min(gen_dist_msb_masked(current_write, current_free)); //NOTE: If current_write - current_free > buffer_capacity, that's because current_free hasn't caught up yet, since we never allow writers to lap readers by more than buffer_capacity. In that case, be conservative and report 0 available slots.
        let actual_n = (n as NumericType).min(available);

        if actual_n == 0 {
            return Ok((0, 0, 0));
        }

        if core::mem::size_of::<E>() == 0 { // CAS case
            // Try to CAS write_gen from current_write to current_write + actual
            let expected_new_write = gen_add_msb_masked(current_write, actual_n);
            // See "Important Note" from try_reserve_slots above.
            match self.write_gen.compare_exchange(
                current_write,
                expected_new_write,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => Ok((current_write, expected_new_write, actual_n as usize)),
                Err(_) => Err(ReserveError::FailedGrab),
            }
        } else {
            // Update write_gen from current_write to current_write + actual
            let new_write_gen = gen_add_msb_masked(current_write, actual_n);
            // See "Important Note" from try_reserve_slots above.
            self.write_gen.store(new_write_gen, Ordering::Release);

            Ok((current_write, new_write_gen, actual_n as usize))
        }
    }

    /// Publish slots in the range [start_generation, end_generation) for readers, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// This must be called with exactly the range returned by try_reserve_slots.
    fn publish_slots<E>(
        &self,
        start_generation: NumericType,
        end_generation: NumericType,
    ) -> Result<(), PublishError> {
        if gen_gte_msb_masked(start_generation, end_generation) {
            return Ok(());
        }

        if core::mem::size_of::<E>() == 0 { // CAS case
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
        } else {
            if self.load_read_gen() != start_generation {
                return Err(PublishError::OutOfOrderPublishAttempt);
            }
            // Update read_gen from start_generation to end_generation
            self.read_gen.store(end_generation, Ordering::Release);
            Ok(())
        }
    }

    /// Register a new reader, returning a unique reader ID.
    /// Uses CAS to enter from suspended state, retries on CAS failure.
    #[cfg(feature = "std")]
    fn register_reader(&self) -> Result<NumericType, RegisterError> {
        // Generate a random reader ID using thread RNG
        let mut rng = rand::rng();
        let seed = rng.random::<NumericType>();

        self.register_reader_with_seed(seed)
    }

    /// Register a new reader, with the provided reader ID seed. Note that the provided reader ID will be used to generate a unique reader ID, which is what will actually be used.
    /// Uses CAS to enter from suspended state, retries on CAS failure.
    fn register_reader_with_seed(&self, mut seed: NumericType) -> Result<NumericType, RegisterError> {
        // Ensure seed doesn't have MSB set
        seed = seed & !MSB;
        if seed == 0 {
            seed = 1;
        }

        // Use folded insertion to get a unique reader ID
        unsafe {
            let (ptr, is_new, _, reader_id_raw, index): (*const CachePadded<AtomicType>, _, _, _, _) = self.reader_gens.get_or_insert_concurrent(
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
            debug_assert!(reader_id != 0 && (reader_id & MSB) == 0);

            // Initialize the CachePadded<AtomicType>
            // Pre-initialized value is SUSPENDED_BIT (or dragged forward by cleanup), we'll CAS it to entry generation
            // We MUST NOT overwrite the value here, as it may contain a forced update from cleanup.
            // ptr.write(CachePadded::new(AtomicType::new(SUSPENDED_BIT)));
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
        reader_id: NumericType,
        func_name: &str,
    ) -> Option<*const CachePadded<AtomicType>> {
        unsafe {
            let (ptr, is_new, _, _, _): (*const CachePadded<AtomicType>, _, _, _, _) = self.reader_gens.get_or_insert_concurrent(
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
    fn get_reader_gen(&self, reader_id: NumericType) -> Option<NumericType> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "get_reader_gen")?;

            let generation = (**ptr).load(Ordering::Acquire);
            Some(generation)
        }
    }

    /// Try to set the reader's generation number (for advancing); fails if the reader is suspended or if the new_generation is older or equal to the current generation (this is not allowed since the contract is that readers acknowledge any generations older than their current stated generation is not guaranteed to still exist, so setting to an older generation would be lying and could cause undefined behavior) (the equal case also errors to save on an equality check).
    fn set_reader_gen(&self, reader_id: NumericType, new_generation: NumericType) -> Result<(), SetGenError> {
        unsafe {
            let ptr = self.get_reader_ptr_boilerplate(reader_id, "set_reader_gen").ok_or(SetGenError::ReaderNotFound)?;

            let current = (**ptr).load(Ordering::Acquire);

            // If suspended, we DO NOT own the slot anymore (cleanup thread might be updating it),
            // so we must fail.
            if is_suspended(current) {
                return Err(SetGenError::ReaderSuspended);
            }

            // Don't allow setting to a value less than current (this is non-negotiable, since the contract is that readers acknowledge any generations older than their current stated generation is not guaranteed to still exist)
            if gen_lte_msb_masked(new_generation, current) {
                return Err(SetGenError::InvalidGeneration);
            }

            // For simplicity, just do a simple store since reader owns this
            (**ptr).store(new_generation, Ordering::Release);
            Ok(())
        }
    }

    /// Mark a reader as suspended by setting the MSB on their generation number
    fn suspend_reader(&self, reader_id: NumericType) -> Result<(), SuspendError> {
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
    fn resume_reader(&self, reader_id: NumericType) -> Result<bool, ResumeError> {
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
    fn resume_reader_at(&self, reader_id: NumericType, entry_generation: NumericType) -> Result<bool, ResumeError> {
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
                if gen_lt_msb_masked(entry_generation, current_gen_without_suspend) {
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
    fn unregister_reader(&self, reader_id: NumericType) -> Result<(), UnregisterError> {
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
    /// and attempts to update last_valid_gen, using CAS if `E = ()`, or writing directly if `E = Exclusive`, assuming this thread is guaranteed to be the only one currently cleaning.
    fn try_cleanup_old_slots<E>(&self) -> Result<CleanupResult, CleanupError> {
        let mut last_valid = self.load_last_valid_gen();
        let read_gen = self.load_read_gen();

        if gen_gte_msb_masked(last_valid, read_gen) {
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

                    // Force the reader forward if our current min_held_gen is greater than this reader's generation
                    if gen_gt_msb_masked(min_held_gen, current_gen_without_suspend) {
                        // The new re-entry minimum we want to advance suspended readers to
                        let forced_update = min_held_gen;

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
                    } else {
                        continue 'outer;
                    }
                }

                // Reader is active, so track their generation
                // Update min_held_gen if our current min_held_gen is greater than this reader's generation
                if gen_gte_msb_masked(min_held_gen, reader_gen) {
                    min_held_gen = reader_gen;
                }
            }
        }

        let new_last_valid = min_held_gen;

        // Loop while we can make progress
        loop {
            if gen_lte_msb_masked(new_last_valid, last_valid) {
                // Either there was nothing to clean, or if we are on the second iteration and beyond, other threads have cleaned everything we wanted to clean.
                return Err(CleanupError::NothingToClean);
            }
            // Since new_last_valid > last_valid/actual, there's more to clean, so try to do that.
            
            match if core::mem::size_of::<E>() == 0 { // CAS case
                // Try to CAS last_valid_gen:
                self.last_valid_gen.compare_exchange(
                    last_valid,
                    new_last_valid,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
            } else {
                assert!(self.load_last_valid_gen() == last_valid, "last_valid_gen should not have changed while we were working if we are the only thread cleaning up.");
                // Update last_valid_gen:
                self.last_valid_gen.store(new_last_valid, Ordering::Release);
                Ok(0)
            } {
                Ok(_) => {
                    // Successfully updated. Now we need to drop items in [last_valid, new_last_valid)
                    let slots_cleaned = gen_dist_msb_masked(new_last_valid, last_valid);

                    // Drop items if T needs drop
                    if core::mem::needs_drop::<T>() {
                        for generation_offset in 0..slots_cleaned {
                            let generation = gen_add_msb_masked(last_valid, generation_offset);
                            unsafe {
                                let ptr = self.get_slot_ptr_mut(generation);
                                ptr::drop_in_place(ptr);
                            }
                        }
                    }

                    // Update free_gen to allow writers to reuse the slots we just freed
                    #[cfg(debug_assertions)]
                    let mut tr = 0;
                    loop {
                        #[cfg(debug_assertions)]
                        {
                            tr += 1;
                            if tr >= SPIN_LIMIT {
                                panic!("SPIN_LIMIT reached! (try_cleanup_old_slots free_gen update, waiting for another cleanup thread to finish; cleanup threads must finish sequentially, so slow cleanup threads can block other cleanup threads from exiting even if they've finished their own cleanup work)");
                            }
                        }

                        if core::mem::size_of::<E>() == 0 { // CAS case
                            match self.free_gen.compare_exchange(
                                last_valid,
                                new_last_valid,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            ) {
                                Ok(_) => break,
                                Err(_) => core::hint::spin_loop(), // Loop again to try updating free_gen since another cleanup thread is still working
                            }
                        } else {
                            assert!(self.load_free_gen() == last_valid, "free_gen should not have changed while we were working if we are the only thread cleaning up.");
                            // Update free_gen:
                            self.free_gen.store(new_last_valid, Ordering::Release);
                            break;
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
pub struct AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    inner: Area,
    phantom: PhantomData<(T, SReaderGens, SBuf, SAtomicUsizeCounter)>,
}

unsafe impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Send for AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    T: Send,
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}
unsafe impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Sync for AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    T: Send,
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>> + Copy + Clone,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Create a new writer handle for the same area
    #[inline]
    pub fn create_writer(&self) -> Self {
        unsafe {
            let inner = self.inner.as_ref();
            inner.writers_count.fetch_add(1, Ordering::AcqRel);
            AreaWriter { inner: self.inner, phantom: PhantomData }
        }
    }
}

pub trait AreaWriterTrait<T> {
    /// Try to reserve n slots for writing, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// Returns (start_generation, end_generation) on success.
    /// end_generation is exclusive (so the range is [start_generation, end_generation)).
    fn try_reserve_slots<E>(&self, n: usize) -> Result<(NumericType, NumericType), ReserveError>;

    /// Try to reserve up to n slots, getting as many as possible, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// Returns (start_generation, end_generation, actual_count) or [`ReserveError::FailedGrab`] on race (only when `E = ()`).
    /// Never returns [`ReserveError::NoSpace`], since in the case that no slots are available it simply returns `Ok((0, 0, 0))`.
    fn try_reserve_slots_best_effort<E>(
        &self,
        n: usize,
    ) -> Result<(NumericType, NumericType, usize), ReserveError>;

    /// Publish slots in the range [start_generation, end_generation) for readers, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
    /// This must be called with exactly the range returned by try_reserve_slots.
    fn publish_slots<E>(
        &self,
        start_generation: NumericType,
        end_generation: NumericType,
    ) -> Result<(), PublishError>;

    /// Get a mutable pointer to the slot at the given generation number.
    ///
    /// # Safety
    /// - generation must be within a range you reserved via try_reserve_slots
    /// - You must initialize the slot before calling publish_slots
    /// - You MUST NOT access the slot after publishing (use a separate AreaReader for that)
    unsafe fn get_slot_ptr(&self, generation: NumericType) -> *mut T;
}

macro_rules! impl_writer_functionality {
    ( $($name:ident),* ) => { // Matches one or more identifiers separated by commas
        $( // Starts a repetition for each identifier captured by `$name`
            impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaWriterTrait<T> for $name<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
            where 
                Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
                SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
                SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
                SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
            {
                /// Try to reserve n slots for writing, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
                /// Returns (start_generation, end_generation) on success.
                /// end_generation is exclusive (so the range is [start_generation, end_generation)).
                #[inline]
                fn try_reserve_slots<E>(&self, n: usize) -> Result<(NumericType, NumericType), ReserveError> {
                    unsafe { self.inner.as_ref().try_reserve_slots::<E>(n) }
                }

                /// Try to reserve up to n slots, getting as many as possible, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
                /// Returns (start_generation, end_generation, actual_count) or [`ReserveError::FailedGrab`] on race (only when `E = ()`).
                /// Never returns [`ReserveError::NoSpace`], since in the case that no slots are available it simply returns `Ok((0, 0, 0))`.
                #[inline]
                fn try_reserve_slots_best_effort<E>(
                    &self,
                    n: usize,
                ) -> Result<(NumericType, NumericType, usize), ReserveError> {
                    unsafe { self.inner.as_ref().try_reserve_slots_best_effort::<E>(n) }
                }

                /// Publish slots in the range [start_generation, end_generation) for readers, using CAS if `E = ()`, or writing directly assuming this writer is the only one active if `E = Exclusive`.
                /// This must be called with exactly the range returned by try_reserve_slots.
                #[inline]
                fn publish_slots<E>(
                    &self,
                    start_generation: NumericType,
                    end_generation: NumericType,
                ) -> Result<(), PublishError> {
                    unsafe {
                        self.inner
                            .as_ref()
                            .publish_slots::<E>(start_generation, end_generation)
                    }
                }

                /// Get a mutable pointer to the slot at the given generation number.
                ///
                /// # Safety
                /// - generation must be within a range you reserved via try_reserve_slots
                /// - You must initialize the slot before calling publish_slots
                /// - You MUST NOT access the slot after publishing (use a separate AreaReader for that)
                #[inline]
                unsafe fn get_slot_ptr(&self, generation: NumericType) -> *mut T {
                    unsafe { self.inner.as_ref().get_slot_ptr_mut(generation) }
                }
            }

            impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> $name<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
            where 
                Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>> + Copy + Clone,
                SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
                SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
                SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
            {
                /// Create a reservation for n slots.
                /// Returns an RAII guard that must be published.
                /// 
                /// If this writer can be guaranteed to be the only active one at any moment, use `E = Exclusive` instead of `E = ()` to avoid the overhead of CAS. Consider if the performance benefits are necessary. If not, it might be safer to just use `E = ()` to allow for multiple concurrent writers anyways, though the single writer case *is* one of the best cases for this library.
                pub fn reserve<E>(&self, n: usize) -> Result<Reservation<'_, Self, T>, ReserveError> {
                    let (start, end) = self.try_reserve_slots::<E>(n)?;
                    Ok(Reservation {
                        writer: self,
                        start_gen: start,
                        end_gen: end,
                        published: false,
                        phantom: PhantomData,
                    })
                }

                /// Try to reserve up to n slots, getting as many as possible.
                /// Returns an RAII guard that must be published.
                /// 
                /// If there are no slots available, this does not return [`ReserveError::NoSpace`] but instead simply returns an empty [`Reservation`].
                /// 
                /// If this writer can be guaranteed to be the only active one at any moment, use `E = Exclusive` instead of `E = ()` to avoid the overhead of CAS. Consider if the performance benefits are necessary. If not, it might be safer to just use `E = ()` to allow for multiple concurrent writers anyways, though the single writer case *is* one of the best cases for this library.
                pub fn reserve_best_effort<E>(&self, n: usize) -> Result<Reservation<'_, Self, T>, ReserveError> {
                    let (start, end, count) = self.try_reserve_slots_best_effort::<E>(n)?;
                    debug_assert!(gen_dist_msb_masked(end, start) == count as NumericType);
                    Ok(Reservation {
                        writer: self,
                        start_gen: start,
                        end_gen: end,
                        published: false,
                        phantom: PhantomData,
                    })
                }
            }

            impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> $name<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
            where 
                Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
                SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
                SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
                SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
            {
                /// Get the number of active writers
                pub fn get_writers_count(&self) -> usize {
                    unsafe { self.inner.as_ref().load_writers_count() }
                }
            }
        )* // Ends the repetition
    };
}

impl_writer_functionality!(AreaWriter, AreaReader);

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Try to cleanup old slots by advancing last_valid_gen.
    /// This scans ALL reader generation slots (including empty ones), force-advances suspended readers,
    /// and attempts to update last_valid_gen, using CAS if `E = ()`, or writing directly if `E = Exclusive`, assuming this thread is guaranteed to be the only one currently cleaning.
    #[inline]
    pub fn try_cleanup_old_slots<E>(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.inner.as_ref().try_cleanup_old_slots::<E>() }
    }
}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Drop for AreaWriter<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
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
                    let _ = inner.try_cleanup_old_slots::<Exclusive>(); // We can use the exclusive version here since we are the only thread left at this point. If we are free to drop the atomics, then we can definitely modify them without coordination as well.

                    // Free the leaked atomics
                    self.inner.as_ref().destroy_stages.deallocate();
                    self.inner.as_ref().reader_keep_alloc_tickets.deallocate();
                    self.inner.as_ref().reader_stage_tickets.deallocate();

                    // Free the buffer
                    self.inner.as_ref().buffer.deallocate();

                    // Free the AreaInner
                    self.inner.deallocate();
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
    pub(crate) writer: &'a W,
    pub(crate) start_gen: NumericType,
    pub(crate) end_gen: NumericType,
    pub(crate) published: bool,
    phantom: PhantomData<T>,
}

impl<'a, W, T> Reservation<'a, W, T>
where
    W: AreaWriterTrait<T>
{
    /// Get the number of reserved slots
    pub fn len(&self) -> usize {
        gen_dist_msb_masked(self.end_gen, self.start_gen) as usize
    }

    /// Check if the reservation is empty
    pub fn is_empty(&self) -> bool {
        gen_gte_msb_masked(self.start_gen, self.end_gen)
    }

    /// Get a mutable reference to the slot at the given index within the reservation.
    /// Returns `&mut MaybeUninit<T>`, since by definition the slot is allocated but uninitialized.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut MaybeUninit<T>> {
        if index >= self.len() {
            return None;
        }
        let generation = gen_add_msb_masked(self.start_gen, index as NumericType);
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
    /// If this writer can be guaranteed to be the only active one at any moment, use `E = Exclusive` instead of `E = ()` to avoid the overhead of CAS. Consider if the performance benefits are necessary. If not, it might be safer to just use `E = ()` to allow for multiple concurrent writers anyways, though the single writer case *is* one of the best cases for this library.
    /// 
    /// *Only fails when trying to publish out-of-order in the `E = Exclusive` case.*
    /// 
    /// # Safety
    /// This function is marked unsafe since it is the point at which the messages go out, and readers can access it with the assumption that all slots are initialized.
    /// If you have not initialized all slots in the reservation before calling this, readers will see uninitialized data and cause undefined behavior.
    pub unsafe fn publish<E>(mut self) -> Result<(), (Self, PublishError)> {
        self.published = true;
        match self.writer.publish_slots::<E>(self.start_gen, self.end_gen) {
            Ok(_) => Ok(()),
            Err(err) => {
                self.published = false;
                Err((self, err))
            },
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

        #[cfg(debug_assertions)]
        let mut tr = 0;
        while let Err((returned, err)) = unsafe { reservation.publish::<()>() } {
            #[cfg(debug_assertions)]
            {
                tr += 1;
                if tr >= SPIN_LIMIT {
                    panic!("SPIN_LIMIT reached! (publish_spin)");
                }
            }

            reservation = returned;
            core::hint::spin_loop();

            match err {
                PublishError::CasFailed => continue,
                PublishError::OutOfOrderPublishAttempt => {
                    unreachable!("OutOfOrderPublishAttempt should never happen in publish_spin since the range check is implicit in the CAS failure. There are no separate checks for this since there's no clear way to distinguish an out-of-order publish attempt from normal publishing failure due to correct coordination with other writers.")
                },
            }
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

        let split_gen = gen_add_msb_masked(self.start_gen, index as NumericType);

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
pub struct AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    inner: Area,
    reader_id: NumericType,
    phantom: PhantomData<(T, SReaderGens, SBuf, SAtomicUsizeCounter)>,
}

unsafe impl<T: Send, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Send for AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> 
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}
unsafe impl<T: Send, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Sync for AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> 
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>> + Copy + Clone,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Create a new reader handle for the same area
    #[cfg(feature = "std")]
    pub fn create_reader(&self) -> Result<Self, RegisterError> {
        unsafe {
            let inner = self.inner.as_ref();
            let reader_id = inner.register_reader()?;
            Ok(AreaReader {
                inner: self.inner,
                reader_id,
                phantom: PhantomData,
            })
        }
    }

    /// Create a new reader handle for the same area
    /// 
    /// **Seed must not be 0, or have the MSB set.**
    /// 
    /// Uses the provided seed ID for reader ID generation. The actual ID will be a unique one derived from the seed.
    pub fn create_reader_with_seed(&self, seed: NumericType) -> Result<Self, RegisterError> {
        unsafe {
            let inner = self.inner.as_ref();
            let reader_id = inner.register_reader_with_seed(seed)?;
            Ok(AreaReader {
                inner: self.inner,
                reader_id,
                phantom: PhantomData,
            })
        }
    }
}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Get the current generation number for this reader, including the suspended bit if it's set
    pub fn get_gen(&self) -> NumericType {
        unsafe {
            self.inner
                .as_ref()
                .get_reader_gen(self.reader_id)
                .expect("Reader not found! This should not happen.")
        }
    }

    /// Load the current published read generation
    pub fn load_read_gen(&self) -> NumericType {
        unsafe { self.inner.as_ref().load_read_gen() }
    }

    /// Try to set the reader's generation number (for advancing); fails if the reader is suspended or if the new_generation is older or equal to the current generation (this is not allowed since the contract is that readers acknowledge any generations older than their current stated generation is not guaranteed to still exist, so setting to an older generation would be lying and could cause undefined behavior) (the equal case also errors to save on an equality check).
    fn set_reader_gen(&mut self, new_generation: NumericType) -> Result<(), SetGenError> {
        unsafe {
            self.inner
                .as_ref()
                .set_reader_gen(self.reader_id, new_generation)
        }
    }

    /// Advance this reader's generation by n messages
    fn advance(&mut self, n: NumericType) -> Result<(), SetGenError> {
        let current = self.get_gen();
        let new_generation = gen_add_msb_masked(gen_without_suspended_bit(current), n);
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
    unsafe fn get_slot_ptr(&self, generation: NumericType) -> *const T {
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
    pub fn resume_at(&mut self, entry_generation: NumericType) -> Result<bool, ResumeError> {
        unsafe {
            self.inner
                .as_ref()
                .resume_reader_at(self.reader_id, entry_generation)
        }
    }

    /// Try to cleanup old slots by advancing last_valid_gen.
    /// This scans ALL reader generation slots (including empty ones), force-advances suspended readers,
    /// and attempts to update last_valid_gen, using CAS if `E = ()`, or writing directly if `E = Exclusive`, assuming this thread is guaranteed to be the only one currently cleaning.
    #[inline]
    pub fn try_cleanup_old_slots<E>(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.inner.as_ref().try_cleanup_old_slots::<E>() }
    }
}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Drop for AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
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

            // Step 9: We're attempting to free, wait for reader_keep_alloc_tickets to hit 0
            #[cfg(debug_assertions)]
            let mut tr = 0;
            while keep_alloc.load(Ordering::Acquire) != 0 {
                #[cfg(debug_assertions)]
                {
                    tr += 1;
                    if tr >= SPIN_LIMIT {
                        panic!("SPIN_LIMIT reached! (AreaReader drop, waiting for reader_keep_alloc_tickets to hit 0)");
                    }
                }

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
                    let _ = inner.try_cleanup_old_slots::<Exclusive>(); // We can use the exclusive version here since we are the only thread left at this point. If we are free to drop the atomics, then we can definitely modify them without coordination as well.

                    // Free the leaked atomics
                    self.inner.as_ref().destroy_stages.deallocate();
                    self.inner.as_ref().reader_keep_alloc_tickets.deallocate();
                    self.inner.as_ref().reader_stage_tickets.deallocate();

                    // Free the buffer
                    self.inner.as_ref().buffer.deallocate();

                    // Free the AreaInner
                    self.inner.deallocate();                }
            }
        }
    }
}

/// A slice-like view of readable messages that auto-advances on drop
pub struct ReadSlice<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    reader: &'a mut AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>,
    start_gen: NumericType,
    end_gen: NumericType,
    armed: bool,
}

impl<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> ReadSlice<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Create a new ReadSlice that is disarmed by default (won't auto-advance on drop).
    /// Caller must manually advance.
    ///
    /// # Safety
    /// - start_gen and end_gen must be valid generation numbers
    pub unsafe fn new_disarmed(
        reader: &'a mut AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>,
        start_gen: NumericType,
        end_gen: NumericType,
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
    pub unsafe fn into_raw_parts(self) -> (&'a mut AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>, NumericType, NumericType, bool) {
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
        gen_dist_msb_masked(self.end_gen, self.start_gen) as usize
    }

    /// Check if the slice is empty
    pub fn is_empty(&self) -> bool {
        gen_gte_msb_masked(self.start_gen, self.end_gen)
    }

    /// Get a reference to the message at the given index
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        let generation = gen_add_msb_masked(self.start_gen, index as NumericType);
        unsafe {
            let ptr = self.reader.get_slot_ptr(generation);
            Some(&*ptr)
        }
    }

    /// Iterate over all messages in the slice
    pub fn iter(&self) -> ReadSliceIter<'_, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> {
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
        let len = gen_dist_msb_masked(self.end_gen, self.start_gen) as usize;

        unsafe {
            let buffer_ptr = self.reader.inner.as_ref().buffer.as_ptr().as_ptr() as *const T;

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

    /// Try to cleanup old slots by advancing last_valid_gen.
    /// This scans ALL reader generation slots (including empty ones), force-advances suspended readers,
    /// and attempts to update last_valid_gen, using CAS if `E = ()`, or writing directly if `E = Exclusive`, assuming this thread is guaranteed to be the only one currently cleaning.
    #[inline]
    pub fn try_cleanup_old_slots<E>(&self) -> Result<CleanupResult, CleanupError> {
        unsafe { self.reader.inner.as_ref().try_cleanup_old_slots::<E>() }
    }
}

impl<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Drop for ReadSlice<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
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
        let _ = self.reader.set_reader_gen(self.end_gen);
    }
}

impl<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> AreaReader<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    /// Get a slice of all currently available messages.
    /// When the slice is dropped, the reader automatically advances past these messages.
    pub fn read(&mut self) -> ReadSlice<'_, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> {
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
    pub fn read_with_check(&mut self) -> Result<ReadSlice<'_, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>, ReadError> {
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
pub struct ReadSliceIter<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    slice: &'a ReadSlice<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>,
    current: usize,
}

impl<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> Iterator for ReadSliceIter<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
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

impl<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter> ExactSizeIterator for ReadSliceIter<'a, T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>>,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{}

/// Create a new area with the given buffer and reader capacity.
/// Returns a writer and a reader handle.
/// Panics if initial reader registration fails (e.g. capacity reached).
#[cfg(any(feature = "std", feature = "alloc"))]
pub fn area<T>(buffer_capacity: usize, reader_capacity: usize) -> (
    AreaWriter<
        T,
        BoxedStorage<AreaInner<
            T,
            BoxedSliceStorage<Slot<CachePadded<AtomicType>>>,
            BoxedSliceStorage<UnsafeCell<MaybeUninit<T>>>,
            BoxedStorage<AtomicUsize>,
        >>,
        BoxedSliceStorage<Slot<CachePadded<AtomicType>>>,
        BoxedSliceStorage<UnsafeCell<MaybeUninit<T>>>,
        BoxedStorage<AtomicUsize>
    >,
    AreaReader<
        T,
        BoxedStorage<AreaInner<
            T,
            BoxedSliceStorage<Slot<CachePadded<AtomicType>>>,
            BoxedSliceStorage<UnsafeCell<MaybeUninit<T>>>,
            BoxedStorage<AtomicUsize>,
        >>,
        BoxedSliceStorage<Slot<CachePadded<AtomicType>>>,
        BoxedSliceStorage<UnsafeCell<MaybeUninit<T>>>,
        BoxedStorage<AtomicUsize>
    >
) {
    let inner = AreaInner::new(buffer_capacity, reader_capacity);

    unsafe {
        let inner_ref = inner.as_ref();
        // Set writers count to 1 (the returned writer)
        inner_ref.writers_count.store(1, Ordering::Release);

        // Register the initial reader
        let reader_id = match inner_ref.register_reader_with_seed(1) {
            Ok(id) => id,
            Err(RegisterError::ReaderCapacityReached) => {
                panic!("Initial reader registration failed: capacity reached")
            }
        };

        let writer = AreaWriter { inner, phantom: PhantomData };
        let reader = AreaReader { inner, reader_id, phantom: PhantomData };

        (writer, reader)
    }
}

pub fn finish_init<T, Area, SReaderGens, SBuf, SAtomicUsizeCounter>(inner: Area) -> (
    AreaWriter<
        T,
        Area,
        SReaderGens,
        SBuf,
        SAtomicUsizeCounter,
    >,
    AreaReader<
        T,
        Area,
        SReaderGens,
        SBuf,
        SAtomicUsizeCounter,
    >
) 
where 
    Area: FixedStorage<AreaInner<T, SReaderGens, SBuf, SAtomicUsizeCounter>> + Clone,
    SReaderGens: FixedStorage<Slot<CachePadded<AtomicType>>> + FixedStorageMultiple<Slot<CachePadded<AtomicType>>>,
    SBuf: FixedStorage<UnsafeCell<MaybeUninit<T>>> + FixedStorageMultiple<UnsafeCell<MaybeUninit<T>>>,
    SAtomicUsizeCounter: FixedStorage<AtomicUsize>,
{
    unsafe {
        let inner_ref = inner.as_ref();
        // Set initial writers count
        inner_ref.writers_count.store(1, Ordering::Release);

        // Register the initial reader
        let reader_id = match inner_ref.register_reader_with_seed(1) {
            Ok(id) => id,
            Err(RegisterError::ReaderCapacityReached) => {
                panic!("Initial reader registration failed: capacity reached")
            }
        };

        let writer = AreaWriter { inner: inner.clone(), phantom: PhantomData };
        let reader = AreaReader { inner, reader_id, phantom: PhantomData };

        (writer, reader)
    }
}

#[cfg(test)]
#[cfg(any(feature = "std", feature = "alloc"))]
mod tests {
    use super::*;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    extern crate alloc;
    #[cfg(feature = "std")]
    extern crate std as alloc;

    #[cfg(any(feature = "std", feature = "alloc"))]
    use alloc::vec::Vec;

    #[test]
    fn test_basic_writer_reserve_and_publish() {
        let (writer, reader) = area::<u64>(16, 8);

        // Initial state
        assert_eq!(reader.get_gen(), 0);
        // Reserve 4 slots
        let (start, end) = writer.try_reserve_slots::<()>(4).expect("Failed to reserve");
        assert_eq!(start, 0);
        assert_eq!(end, 4);

        // Write to the slots
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64 * 10);
            }
        }

        // Publish the slots
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

        // Verify read_gen moved forward
        unsafe {
            let read_gen = writer.inner.as_ref().load_read_gen();
            assert_eq!(read_gen, 4);
        }
    }

    #[test]
    fn test_reservation_write_and_publish() {
        let (writer, _reader) = area::<u64>(16, 8);

        // Reserve 4 slots
        let mut reservation = writer.reserve::<()>(4).unwrap();
        
        // Write to reservation
        for i in 0..4 {
            reservation.get_mut(i).unwrap().write(100 + i as u64);
        }
        
        unsafe { reservation.publish::<()>() }.unwrap_or_else(|_| panic!("Failed to publish reservation"));
    }

    #[test]
    fn test_reservation_split() {
        let (writer, _reader) = area::<u64>(16, 8);

        // Reserve 4 slots
        let reservation = writer.reserve::<()>(4).unwrap();
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
        match unsafe { right.publish::<()>() } {
            Ok(_) => panic!("Wrong order works; there's a bug"),
            Err((right, _)) => {
                assert!(unsafe { left.publish::<()>() }.is_ok());
                assert!(unsafe { right.publish::<()>() }.is_ok());
            },
        }
    }

    #[test]
    fn test_reservation_raii() {
        let (writer, reader) = area::<u64>(16, 8);

        // Use reservation
        let mut reservation = writer.reserve::<()>(4).expect("Failed to reserve");
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

        let (writer, mut reader) = area::<ComplexStruct>(16, 8);

        let mut reservation = writer.reserve::<()>(1).unwrap();
        let slot = reservation.get_mut(0).unwrap();

        // In-place initialization using raw pointers
        let ptr = slot.as_mut_ptr();
        unsafe {
            (&raw mut (*ptr).a).write(42);
            (&raw mut (*ptr).b).write(alloc::vec![1, 2, 3]);
        }

        unsafe { reservation.publish::<()>() }.unwrap_or_else(|_| panic!("Failed to publish reservation"));

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
        let _reservation = writer.reserve::<()>(4).expect("Failed to reserve");
        // Drop without publish -> panic
    }

    #[test]
    fn test_reader_registration_and_reading() {
        let (writer, reader) = area::<u64>(16, 8);

        // Write some messages
        let (start, end) = writer.try_reserve_slots::<()>(3).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64 + 100);
            }
        }
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

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
                assert_eq!(value, generation as u64 + 100);
            }
        }
    }

    #[test]
    fn test_register_capacity_reached() {
        let (_writer, reader1) = area::<u64>(16, 2); // Small capacity

        // Capacity is 2. reader1 takes one slot.

        // Create another reader
        let _reader2 = reader1.create_reader_with_seed(100).expect("Should succeed");

        // Now capacity should be full (2 readers)
        // Trying to create a 3rd reader should fail
        let result = reader1.create_reader_with_seed(101);
        assert_eq!(result.err(), Some(RegisterError::ReaderCapacityReached));
    }

    #[test]
    fn test_create_writer() {
        let (writer1, _reader) = area::<u64>(16, 8);
        let writer2 = writer1.create_writer();

        // Both writers should work
        let (start1, end1) = writer1.try_reserve_slots::<()>(2).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 2);

        let (start2, end2) = writer2.try_reserve_slots::<()>(2).expect("Failed to reserve");
        assert_eq!(start2, 2);
        assert_eq!(end2, 4);
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_create_reader() {
        let (writer, reader1) = area::<u64>(16, 8);
        let reader2 = reader1.create_reader().expect("Failed to create reader");

        // Different reader IDs
        assert_ne!(reader1.reader_id, reader2.reader_id);

        // Write some messages
        let (start, end) = writer.try_reserve_slots::<()>(3).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64 * 2);
            }
        }
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

        // Both readers can read
        let read_gen = reader1.load_read_gen();
        assert_eq!(read_gen, 3);
        assert_eq!(reader2.load_read_gen(), 3);
    }

    #[test]
    fn test_reserve_overflow() {
        let (writer, _reader) = area::<u64>(4, 8);

        // Fill the buffer
        let (start1, end1) = writer.try_reserve_slots::<()>(4).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 4);

        // Try to reserve more - should fail
        let result = writer.try_reserve_slots::<()>(1);
        assert_eq!(result, Err(ReserveError::NoSpace));
    }

    #[test]
    fn test_best_effort_reserve() {
        let (writer, _reader) = area::<u64>(4, 8);

        // Reserve 2 slots normally
        let (start1, end1) = writer.try_reserve_slots::<()>(2).expect("Failed to reserve");
        assert_eq!(start1, 0);
        assert_eq!(end1, 2);

        // Try to reserve 10 slots with best effort - should get only 2
        let (start2, end2, actual) = writer
            .try_reserve_slots_best_effort::<()>(10)
            .expect("Failed to reserve");
        assert_eq!(start2, 2);
        assert_eq!(end2, 4);
        assert_eq!(actual, 2);

        // Try to reserve again, should get 0
        let (start3, end3, actual2) = writer
            .try_reserve_slots_best_effort::<()>(10)
            .expect("Failed to reserve");
        assert_eq!(start3, 0);
        assert_eq!(end3, 0);
        assert_eq!(actual2, 0);

        // Try to reserve more normally, should fail
        assert_eq!(writer.try_reserve_slots::<()>(2), Err(ReserveError::NoSpace));
    }

    #[test]
    fn test_suspend_and_resume() {
        let (_writer, mut reader) = area::<u64>(16, 8);

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
        let (writer, mut reader) = area::<u64>(16, 8);

        // Write some messages
        let (start, end) = writer.try_reserve_slots::<()>(5).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64);
            }
        }
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

        // Reader advances past the messages
        reader.advance(5).expect("Failed to advance");

        // Cleanup should work now
        let result = writer.try_cleanup_old_slots::<()>();
        assert!(result.is_ok());

        let cleanup_result = result.unwrap();
        assert_eq!(cleanup_result.old_last_valid, 0);
        assert_eq!(cleanup_result.new_last_valid, 5);
        assert_eq!(cleanup_result.slots_cleaned, 5);
    }

    #[test]
    fn test_unregister_unblocks_writer() {
        let (writer, reader) = area::<u64>(4, 8);

        // Reader stays at 0

        // Fill the buffer
        let (start, end) = writer.try_reserve_slots::<()>(4).expect("Failed to reserve");
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

        // Writer should be blocked now
        assert_eq!(writer.try_reserve_slots::<()>(1), Err(ReserveError::NoSpace));

        // Unregister the reader (dropping it)
        drop(reader);

        // Trigger cleanup.
        let cleanup_result = writer
            .try_cleanup_old_slots::<()>()
            .expect("Cleanup should succeed");
        assert_eq!(cleanup_result.new_last_valid, 4);

        // Now writer can write
        let (start, end) = writer.try_reserve_slots::<()>(4).expect("Failed to reserve");
        assert_eq!(start, 4);
        assert_eq!(end, 8);
    }

    #[test]
    fn test_writer_drop_decrements_count() {
        let (writer1, _reader) = area::<u64>(16, 8);

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
        let (_writer, mut reader) = area::<u64>(16, 8);

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
        let (_writer, mut reader) = area::<u64>(16, 8);

        assert!(reader.suspend().is_ok());

        // Try to advance while suspended
        let result = reader.advance(1);
        assert_eq!(result, Err(SetGenError::ReaderSuspended));
    }

    #[test]
    fn test_publish_cas_failure() {
        let (writer1, _reader) = area::<u64>(16, 8);
        let writer2 = writer1.create_writer();

        // Writer1 reserves slots 0-2
        let (start1, end1) = writer1.try_reserve_slots::<()>(2).expect("Failed to reserve");
        unsafe {
            for generation in start1..end1 {
                let ptr = writer1.get_slot_ptr(generation);
                ptr.write(generation as u64);
            }
        }

        // Writer2 reserves slots 2-4
        let (start2, end2) = writer2.try_reserve_slots::<()>(2).expect("Failed to reserve");
        unsafe {
            for generation in start2..end2 {
                let ptr = writer2.get_slot_ptr(generation);
                ptr.write(generation as u64);
            }
        }

        // If writer2 tries to publish first, it should fail
        let result = writer2.publish_slots::<()>(start2, end2);
        assert_eq!(result, Err(PublishError::CasFailed));

        // Writer1 publishes successfully
        writer1
            .publish_slots::<()>(start1, end1)
            .expect("Failed to publish");

        // Writer2 publishes successfully
        writer2
            .publish_slots::<()>(start2, end2)
            .expect("Failed to publish");

        // Now if writer1 tries to publish again with stale range, it should fail
        let result = writer1.publish_slots::<()>(start1, end1);
        assert_eq!(result, Err(PublishError::CasFailed));
    }

    #[test]
    fn test_drop_cleanup() {
        // This test verifies that dropping all handles properly cleans up
        // We can't directly observe the cleanup, but we can ensure it doesn't panic
        {
            let (writer, reader) = area::<u64>(16, 8);
            let _writer2 = writer.create_writer();
            let _reader2 = reader.create_reader_with_seed(100).expect("Failed to create reader");

            // Use the handles a bit
            let (start, end) = writer.try_reserve_slots::<()>(2).expect("Failed to reserve");
            unsafe {
                for generation in start..end {
                    let ptr = writer.get_slot_ptr(generation);
                    ptr.write(generation as u64);
                }
            }
            writer.publish_slots::<()>(start, end).expect("Failed to publish");

            // All handles will drop here
        }
        // If we get here without panicking, the cleanup worked
    }

    #[test]
    fn test_read_slice() {
        let (writer, mut reader) = area::<u64>(16, 8);

        // Write some messages
        let (start, end) = writer.try_reserve_slots::<()>(5).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64 * 100);
            }
        }
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

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
        let (start, end) = writer.try_reserve_slots::<()>(2).expect("Failed to reserve");
        unsafe {
            for generation in start..end {
                let ptr = writer.get_slot_ptr(generation);
                ptr.write(generation as u64 * 10);
            }
        }
        writer.publish_slots::<()>(start, end).expect("Failed to publish");

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
        let (writer, mut reader1) = area::<u64>(16, 8);

        // Advance everything to generation 10
        let (start, end) = writer.try_reserve_slots::<()>(10).unwrap();
        unsafe {
            for i in start..end {
                writer.get_slot_ptr(i).write(i as u64);
            }
        }
        writer.publish_slots::<()>(start, end).unwrap();
        reader1.advance(10).unwrap();

        // Cleanup should advance last_valid to 10
        // And it should drag all empty slots to 10
        let result = writer.try_cleanup_old_slots::<()>().unwrap();
        assert_eq!(result.new_last_valid, 10);

        // Now register a new reader
        // It should pick up a slot that was dragged to 10
        // So it should start at 10
        let reader2 = reader1.create_reader_with_seed(100).expect("Failed to create reader");
        assert_eq!(reader2.get_gen(), 10);
    }

    #[test]
    #[should_panic(expected = "reader_capacity must be a power of two")]
    fn test_area_creation_panics_on_invalid_capacity() {
        let (_writer, _reader) = area::<u64>(16, 0);
    }
}
