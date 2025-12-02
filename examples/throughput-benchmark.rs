use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;
use market_square::area::area;
use crossbeam_channel::bounded;

const N_WRITERS: usize = 5; //writers contend for a single atomic; more writers increase contention for that CAS operation
const M_READERS: usize = 5; //market_square scales very well with readers
//each reader and writer constitutes a thread; as with many lock-free algorithms, performance degrade if there are vastly more threads than CPU cores, especially with spin-waiting
const MESSAGES_PER_WRITER: usize = 1_000_000;
const BUFFER_CAPACITY: usize = 16384; //higher capacities reduce waits
const BATCH_SIZE: usize = 64; //try 1, 16, 32; higher batch sizes improve throughput

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
    benchmark_crossbeam();
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
    for _ in 0..M_READERS {
        let mut reader = reader.create_reader().expect("Failed to create reader");
        reader_handles.push(thread::spawn(move || {
            let mut count = 0;
            let total_expected = (N_WRITERS * MESSAGES_PER_WRITER) as u64;

            while let Ok(slice) = reader.read_with_check() {
                let _ = slice.try_cleanup_old_slots();
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
                    match writer.reserve(BATCH_SIZE) {
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

fn benchmark_crossbeam() {
    println!("--- Crossbeam MPMC (Queue) ---");
    
    let (tx, rx) = bounded::<u64>(BUFFER_CAPACITY);
    let start = Instant::now();
    let mut reader_handles = vec![];
    let mut writer_handles = vec![];

    // Spawn readers
    for _ in 0..M_READERS {
        let rx = rx.clone();
        reader_handles.push(thread::spawn(move || {
            let mut count = 0;
            while let Ok(_) = rx.recv() {
                count += 1;
            }
            count
        }));
    }

    // Spawn writers
    let barrier = Arc::new(Barrier::new(N_WRITERS));
    for _ in 0..N_WRITERS {
        let tx = tx.clone();
        let barrier = barrier.clone();
        writer_handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..MESSAGES_PER_WRITER {
                tx.send(i as u64).unwrap();
            }
        }));
    }
    
    drop(tx); // Close the original sender

    for handle in writer_handles {
        handle.join().unwrap();
    }

    let mut total_reads = 0;
    for handle in reader_handles {
        total_reads += handle.join().unwrap();
    }

    let duration = start.elapsed();
    println!("Time: {:?}", duration);
    println!("Total reads: {}", total_reads);
    println!("Throughput: {:.2} million reads/sec", 
        total_reads as f64 / duration.as_secs_f64() / 1_000_000.0);
    println!();
}
