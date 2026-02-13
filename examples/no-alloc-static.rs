//! Example demonstrating `no_alloc` usage with static memory.
//! 
//! Run with: cargo run --example no-alloc-static --no-default-features
//! Compile with (for thumbv7em-none-eabihf target as an example): cargo build --example no-alloc-static --no-default-features --target thumbv7em-none-eabihf

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize};

use market_square::area::{AreaInner, AreaReader, AreaWriter, finish_init};
use market_square::cache_padded::CachePadded;
use market_square::map::{SimpleLPHashMap, Slot, EMPTY};
use market_square::storage::{StaticStorage, StaticSliceStorage};

// Configuration
const BUFFER_CAPACITY: usize = 16;
const READER_CAPACITY: usize = 4; // Must be power of two

// Static storage for the ring buffer
static mut BUFFER: [UnsafeCell<MaybeUninit<u64>>; BUFFER_CAPACITY] = 
    unsafe { MaybeUninit::uninit().assume_init() };

// Static storage for the reader generations map
static mut READER_GENS: MaybeUninit<[Slot<CachePadded<AtomicU64>>; READER_CAPACITY]> = 
    MaybeUninit::uninit();

// Static storage for the coordination atomics
static mut DESTROY_STAGES: AtomicUsize = AtomicUsize::new(2);
static mut READER_KEEP_ALLOC_TICKETS: AtomicUsize = AtomicUsize::new(0);
static mut READER_STAGE_TICKETS: AtomicUsize = AtomicUsize::new(0);

// Static storage for the AreaInner itself
static mut AREA_INNER: MaybeUninit<
    AreaInner<
        u64,
        StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
        StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
        StaticStorage<AtomicUsize>,
    >
> = MaybeUninit::uninit();

/// Initialize all static memory and create writer/reader handles.
/// 
/// # Safety
/// - Must only be called once since the static memory should not be double-initialized
unsafe fn init_area() -> (
    AreaWriter<
        u64,
        StaticStorage<AreaInner<
            u64,
            StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
            StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
            StaticStorage<AtomicUsize>,
        >>,
        StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
        StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
        StaticStorage<AtomicUsize>,
    >,
    AreaReader<
        u64,
        StaticStorage<AreaInner<
            u64,
            StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
            StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
            StaticStorage<AtomicUsize>,
        >>,
        StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
        StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
        StaticStorage<AtomicUsize>,
    >,
) {
    unsafe {
        // Initialize buffer slots
        core::ptr::write(&raw mut BUFFER, [const { UnsafeCell::new(MaybeUninit::uninit()) }; BUFFER_CAPACITY]);

        // Initialize reader_gens slots with SUSPENDED_BIT (required by AreaInner)
        const SUSPENDED_BIT: u64 = 1 << 63;
        core::ptr::write(&raw mut READER_GENS as *mut [Slot<CachePadded<AtomicU64>>; READER_CAPACITY], [const { 
            Slot {
                key: AtomicU64::new(EMPTY),
                value: UnsafeCell::new(MaybeUninit::new(CachePadded::new(AtomicU64::new(SUSPENDED_BIT)))),
            }
        }; READER_CAPACITY]);

        // Create coordination atomics
        DESTROY_STAGES = AtomicUsize::new(2);
        READER_KEEP_ALLOC_TICKETS = AtomicUsize::new(0);
        READER_STAGE_TICKETS = AtomicUsize::new(0);

        // Create static storage wrappers
        let buffer_storage = StaticSliceStorage::from_raw(
            NonNull::new_unchecked(&raw mut BUFFER as *mut UnsafeCell<MaybeUninit<u64>>),
            BUFFER_CAPACITY,
        );
        let reader_gens_storage = StaticSliceStorage::from_raw(
            NonNull::new_unchecked(&raw mut READER_GENS as *mut Slot<CachePadded<AtomicU64>>),
            READER_CAPACITY,
        );
        
        let destroy_stages_storage = StaticStorage::from_raw(
            NonNull::new_unchecked(&raw mut DESTROY_STAGES)
        );
        let reader_keep_alloc_storage = StaticStorage::from_raw(
            NonNull::new_unchecked(&raw mut READER_KEEP_ALLOC_TICKETS)
        );
        let reader_stage_storage = StaticStorage::from_raw(
            NonNull::new_unchecked(&raw mut READER_STAGE_TICKETS)
        );

        // Create the reader_gens map from storage
        let reader_gens_map = SimpleLPHashMap::from_storage(reader_gens_storage);

        // Initialize AreaInner in static memory
        AreaInner::init(
            &raw mut AREA_INNER as *mut AreaInner<
                u64,
                StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
                StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
                StaticStorage<AtomicUsize>,
            >,
            reader_gens_map,
            buffer_storage,
            destroy_stages_storage,
            reader_keep_alloc_storage,
            reader_stage_storage,
        );

        // Create storage wrapper for the AreaInner
        let area_storage = StaticStorage::from_raw(
            NonNull::new_unchecked(&raw mut AREA_INNER as *mut AreaInner<
                u64,
                StaticSliceStorage<Slot<CachePadded<AtomicU64>>>,
                StaticSliceStorage<UnsafeCell<MaybeUninit<u64>>>,
                StaticStorage<AtomicUsize>,
            >)
        );

        finish_init(area_storage)
    }
}

fn main() {
    unsafe {
        let (writer, mut reader) = init_area();

        // Write some messages
        let mut reservation = writer.reserve(3).expect("Failed to reserve");
        reservation.get_mut(0).unwrap().write(100);
        reservation.get_mut(1).unwrap().write(200);
        reservation.get_mut(2).unwrap().write(300);
        reservation.publish_spin();

        // Read messages
        let slice = reader.read();
        println!("Read {} messages", slice.len());
        for (i, val) in slice.iter().enumerate() {
            println!("  [{}] = {}", i, val);
        }
        
        let _ = slice.try_cleanup_old_slots();
        
        println!("done!");
    }
}