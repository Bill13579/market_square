// `throughput-benchmark.rs` adapted for Miri testing.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;
use market_square::area::area;
use market_square::arithmetics::NumericType;

const N_WRITERS: usize = 5; //writers contend for a single atomic; more writers increase contention for that CAS operation
type WriterIsExclusive = (); //more than one writer
// type WriterIsExclusive = market_square::area::Exclusive; //only one writer
const M_READERS: usize = 5; //market_square scales very well with readers
//each reader and writer constitutes a thread; as with many lock-free algorithms, performance degrade if there are vastly more threads than CPU cores, especially with spin-waiting
const MESSAGES_PER_WRITER: usize = 10_000;
const BUFFER_CAPACITY: usize = 128; //higher capacities reduce waits
const BATCH_SIZE: usize = 1; //try 1, 16, 32; higher batch sizes improve throughput

fn main() {
    println!("Benchmarking with:");
    println!("  Writers: {}", N_WRITERS);
    println!("  Readers: {}", M_READERS);
    println!("  Messages per writer: {}", MESSAGES_PER_WRITER);
    println!("  Buffer capacity: {}", BUFFER_CAPACITY);
    println!("  Batch size for market square: {}", BATCH_SIZE);
    println!("  Total messages sent: {}", N_WRITERS * MESSAGES_PER_WRITER);
    println!();

    benchmark_market_square();
}

fn benchmark_market_square() {
    println!("--- Market Square (Broadcast) ---");
    
    // Market Square setup
    let reader_cap = (M_READERS + 1).next_power_of_two().max(8);
    let (writer, mut reader) = area(BUFFER_CAPACITY, reader_cap);

    reader.suspend().expect("Couldn't suspend first reader!");

    let start = Instant::now();
    let mut reader_handles = vec![];
    let mut writer_handles = vec![];

    // Spawn readers
    for i in 0..M_READERS {
        // Use create_reader if you have std, as it will provide better performance through randomized seeding.
        // If you use this though, **seed must not be 0, or have the MSB set.**
        let mut reader = reader.create_reader_with_seed(100 + i as NumericType).unwrap();
        reader_handles.push(thread::spawn(move || {
            let mut count = 0;
            let total_expected = (N_WRITERS * MESSAGES_PER_WRITER) as u64;

            while let Ok(slice) = reader.read_with_check() {
                let _ = slice.try_cleanup_old_slots::<()>();
                count += slice.len() as u64;
            }

            if count != total_expected {
                panic!("Read a different number of messages than expected! expected {}, read {}", total_expected, count);
            }

            count
        }));
    }

    // Spawn writers
    assert!(MESSAGES_PER_WRITER % BATCH_SIZE == 0, "MESSAGES_PER_WRITER must be a multiple of BATCH_SIZE");
    let barrier = Arc::new(Barrier::new(N_WRITERS));
    for _ in 0..N_WRITERS {
        let writer = writer.create_writer();
        let barrier = barrier.clone();
        writer_handles.push(thread::spawn(move || {
            barrier.wait();
            for msg_idx in 0..(MESSAGES_PER_WRITER / BATCH_SIZE) {
                loop {
                    // Writers need to be careful not to begin cleanup before any readers can join. Here, we just don't clean up from the writer-side.
                    // let _ = writer.try_cleanup_old_slots();
                    match writer.reserve::<WriterIsExclusive>(BATCH_SIZE) {
                        Ok(mut reservation) => {
                            for i in 0..BATCH_SIZE {
                                reservation.get_mut(i).unwrap().write((msg_idx * BATCH_SIZE + i) as u64); // Use msg_idx * BATCH_SIZE + i for unique values
                            }
                            unsafe { reservation.publish_spin() };
                            break;
                        }
                        Err(_) => {
                            thread::yield_now();
                        }
                    }
                }
            }
        }));
    }

    drop(reader); // Drop the original reader
    drop(writer); // Drop the original writer

    for handle in writer_handles {
        handle.join().unwrap();
    }
    
    for handle in reader_handles {
        handle.join().unwrap();
    }

    let duration = start.elapsed();
    println!("Time: {:?}", duration);
    println!("Total reads: {}", M_READERS * N_WRITERS * MESSAGES_PER_WRITER);
    println!("Throughput: {:.2} million reads/sec", 
        (M_READERS * N_WRITERS * MESSAGES_PER_WRITER) as f64 / duration.as_secs_f64() / 1_000_000.0);
    println!();
}
