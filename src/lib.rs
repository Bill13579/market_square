#![no_std]

//! [<img src="https://kooworks.org/xathek" alt="Xathek" width="280">](https://kooworks.org/xathek)
//! 
//! # market_square
//! 
//! [![Crates.io](https://img.shields.io/crates/v/market_square)](https://crates.io/crates/market_square)
//! [![Docs.rs](https://img.shields.io/docsrs/market_square)](https://docs.rs/market_square)
//! 
//! This crate provides a lock-free, bounded, multi-producer and multi-consumer "square", where readers and writers can join and leave at any time. It is also built to be able to do so fast.
//! 
//! ## The market square
//! 
//! In a bustling market square, merchants shout openly at anyone nearby about their goods, hoping to catch some sales; and even in such a noisy and energetic place, buyers would then parse all of the things they hear, and get just the things they came there for; although, it's also possible they might find something they forgot they needed, and grab those as well! When it's cheap to do so, having threads being able to read even unrelated messages can create flexibility for the future, for this exact reason.
//! 
//! The goal is to emulate the bustle of the market square. Imagine if you are an `AreaReader`. You walk into a market square where many people are talking (`AreaWriter`s). When you enter the area, you are guaranteed to hear every single message produced since you've entered into that area, which is the hearing range. That message will then be guaranteed to stay alive until such a time when you and all other people who have heard that message decides to forget it.
//! 
//! ## Features
//! 
//! The goal of this crate is high-performance broadcast, but not at the expense of ergonomics *or* power. Here are some notable features:
//! 
//! - Lock-free
//! - Dynamic enter/exit of readers and writers
//! - Readers-are-also-writers, and new readers are dirt cheap to create
//! - Reader suspension so that readers who are momentarily away can avoid holding up the cleaning of old messages
//! - Batched publishing for writers, and batched read for readers
//! - In-place message writing through `MaybeUninit`; you can skip intermediate struct allocation
//! - Any thread can clean-up old messages at any time, up to user in terms of where and when clean-up is done
//! - CPU pre-fetching and cache-locality friendly structures
//! - Ergonomic Rust-y RAII based API for ease-of-use
//! - Maximum control and customization options with low-level control; bring-your-own-locks!
//! - `no_std` **and `no_alloc`** support
//! - `small-gen` feature to use `AtomicU32`s instead of `AtomicU64`s internally, and overflow handling
//! - `no-cache-pad` feature to turn off cache-padding (`align(4)`)
//! - Even faster single-writer-multi-reader case by writing into atomics directly without CAS
//! 
//! ## Usage
//! 
//! Add the following to your `Cargo.toml` file:
//! 
//! ```toml
//! # with std
//! [dependencies]
//! market_square = "0.3"
//! 
//! # with no_std
//! [dependencies]
//! market_square = { version = "0.3", default-features = false, features = ["alloc"] }
//! 
//! # with no_alloc (see examples/no-alloc-static.rs for an example using static memory)
//! [dependencies]
//! market_square = { version = "0.3", default-features = false }
//! ```
//! 
//! Usage is simple; you can create a new Area like so:
//! 
//! ```ignore
//! use market_square::area::area;
//! 
//! let (writer, mut reader) = area(buffer_capacity, reader_capacity);
//! 
//! // `buffer_capacity` is the message ring buffer's capacity, and `reader_capacity` is the maximum number of readers at any given moment, which needs to be a power-of-two.
//! ```
//! 
//! Use `reader.create_reader()` to create more readers, and `writer.create_writer()` to create more writers (creating new readers is fallible if the reader capacity is exceeded).
//! 
//! For clean-up, **`try_cleanup_old_slots()` must be used periodically *somewhere*!!** The power of deciding where to clean-up is entirely in your hands. You can do it inside of the reader threads (which is recommended), or have a completely separate thread that does it (this can cause things to get stuck if you use spin-loops for getting writer slots and the clean-up thread is not scheduled by the OS), or in the writer threads if you wanted to (there's a caveat to this which is noted in both examples). **But you have to make sure it runs *somewhere*, because otherwise you will quickly run out of buffer slots for new messages.**
//! 
//! Suspend readers with `suspend()`, and they will no longer maintain a claim to old messages and prevent clean-up.
//! 
//! Here is a full example of reading and writing messages using separate readers and writers:
//! 
//! ```rust
//! use std::thread;
//! use market_square::area::area;
//! 
//! fn main() {
//!     let buffer_capacity = 20; // This is the ring buffer capacity; messages are stored in it.
//!     let reader_capacity = 16; // This is the maximum number of readers at any given moment. Needs to be a power-of-two.
//! 
//!     let (writer, mut reader) = area(buffer_capacity, reader_capacity);
//! 
//!     reader.suspend().unwrap(); // You can immediately suspend the first reader if you wish. Since all readers must keep reading and moving forward to not hold up clean-up unless they are suspended, suspending the first reader lets you do the usual pattern of cloning from a "template" N times.
//! 
//!     let mut reader_handles = vec![];
//!     let mut writer_handles = vec![];
//! 
//!     // Reader threads
//!     for i in 0..10 {
//!         let mut reader = reader.create_reader().unwrap();
//! 
//!         reader_handles.push(thread::spawn(move || {
//!             while let Ok(slice) = reader.read_with_check() {
//!                 let _ = slice.try_cleanup_old_slots::<()>(); // Readers calling clean-up tends to work well.
//!                 if slice.len() != 0 {
//!                     println!(
//!                         "\n[reader {}] got {} messages! {:?}",
//!                         i,
//!                         slice.len() as u64,
//!                         slice.iter().cloned().collect::<Vec<String>>()
//!                     );
//!                 }
//!             }
//!         }));
//!     }
//! 
//!     // Writer threads
//!     for i in 0..10 {
//!         let writer = writer.create_writer();
//! 
//!         writer_handles.push(thread::spawn(move || {
//!             for msg_idx in 0..8 {
//!                 loop {
//!                     // Writers should be careful not to begin cleanup before any readers can join. Here, we just don't clean up from the writer-side.
//!                     // let _ = writer.try_cleanup_old_slots();
//! 
//!                     match writer.reserve::<()>(1) {
//!                         Ok(mut reservation) => {
//!                             reservation.get_mut(0).unwrap().write(format!("hello from writer {} ({})", i, msg_idx));
//!                             unsafe { reservation.publish_spin(); }
//!                             break;
//!                         }
//!                         Err(_) => {
//!                             thread::yield_now();
//!                         }
//!                     }
//!                 }
//!             }
//!         }));
//!     }
//! 
//!     drop(reader);
//!     drop(writer);
//!     for handle in writer_handles {
//!         handle.join().unwrap();
//!     }
//!     for handle in reader_handles {
//!         handle.join().unwrap();
//!     }
//! 
//!     println!("Done");
//! }
//! ```
//! 
//! And here's how to do it with the readers-are-also-writers pattern, which you might find simpler in many scenarios:
//! 
//! ```rust
//! use std::thread;
//! use market_square::area::area;
//! 
//! // So that all writing starts together.
//! use std::sync::{Arc, Barrier};
//! 
//! fn main() {
//!     let buffer_capacity = 20; // This is the ring buffer capacity; messages are stored in it.
//!     let reader_capacity = 32; // This is the maximum number of participants at any given moment. Needs to be a power-of-two.
//! 
//!     let (_, mut area) = area(buffer_capacity, reader_capacity);
//! 
//!     area.suspend().unwrap(); // See the "hello-world.rs" example for explanation.
//! 
//!     let mut thread_handles = vec![];
//! 
//!     // Spawn threads
//!     let barrier = Arc::new(Barrier::new(20));
//!     for i in 0..20 {
//!         let mut area = area.create_reader().unwrap();
//! 
//!         let expected_msgs = 20 * 8;
//! 
//!         let barrier = barrier.clone();
//!         thread_handles.push(thread::spawn(move || {
//!             let mut msg_idx = 0;
//!             let mut msgs_heard_count = 0;
//!             barrier.wait();
//!             loop {
//!                 if msg_idx < 8 {
//!                     match area.reserve::<()>(1) {
//!                         Ok(mut reservation) => {
//!                             reservation.get_mut(0).unwrap().write(format!("hello from thread {} ({})", i, msg_idx));
//!                             unsafe { reservation.publish_spin(); }
//!                             msg_idx += 1;
//!                         }
//!                         Err(_) => {
//!                             thread::yield_now();
//!                         }
//!                     }
//!                 }
//! 
//!                 // Here, we will hear our own messages. Since we are the ones that wrote the messages, 
//!                 // they are already in our local cache-line, and so reading it back is just like reading 
//!                 // a local variable, meaning checking it adds close to zero overhead.
//!                 // This was intentionally designed this way because of the idea of allowing for more 
//!                 // communication whenever possible and when it is cheap to do so.
//!                 // 
//!                 // Also, when you are shouting in a market square, you always hear what you say yourself!
//!                 let slice = area.read();
//!                 let _ = slice.try_cleanup_old_slots::<()>();
//!                 if slice.len() != 0 {
//!                     println!(
//!                         "\n[thread {}] got {} messages! {:?}",
//!                         i,
//!                         slice.len() as u64,
//!                         slice.iter().cloned().collect::<Vec<String>>()
//!                     );
//!                     msgs_heard_count += slice.len();
//!                 }
//!                 
//!                 if msgs_heard_count == expected_msgs {
//!                     break;
//!                 }
//!             }
//!         }));
//!     }
//! 
//!     drop(area);
//!     for handle in thread_handles {
//!         handle.join().unwrap();
//!     }
//! 
//!     println!("Done");
//! }
//! ```
//! 
//! There is also a more complex benchmarking script which utilizes batching in the examples.
//! 
//! ## Benchmarks
//! 
//! A simple throughput benchmark is available under `examples`, here is an example run on an M1 MacBook Air:
//! 
//! ```text
//! % cargo run --example throughput-benchmark --release
//!     Finished `release` profile [optimized] target(s) in 0.01s
//!      Running `target/release/examples/throughput-benchmark`
//! Benchmarking with:
//!   Writers: 5
//!   Readers: 5
//!   Messages per writer: 1000000
//!   Buffer capacity: 16384
//!   Batch size for market square: 64
//!   Total messages sent: 5000000
//! 
//! --- Market Square (Broadcast) ---
//! Time: 25.910375ms
//! Total reads: 25000000
//! Throughput: 964.86 million reads/sec
//! 
//! --- Crossbeam MPMC (Queue) ---
//! Time: 85.779833ms
//! Total reads: 5000000
//! Throughput: 58.29 million reads/sec
//! ```
//! 
//! Why does market_square seem faster than crossbeam? ***Because they do different things!!***
//! 
//! `crossbeam-channel` is a work queue, and so each sender's message is only given to one of the receivers.
//! 
//! `market_square` is a broadcast system, and so each writer's message is shown to every single reader.
//! 
//! This is why "Total reads" here is higher, since there are, in the case of the example above, for example, technically five times as many reads, where all 5 readers have to have a look at the messages and pass on them before Drop is allowed on those messages.
//! 
//! Being purely broadcasting also provides `market_square` with tricks that a work queue like `crossbeam-channel` might not be able to adopt. For instance, the example above uses batching, where the sender grabs 64 slots at once and publishes them. If we reduce batching to 1 (so, no batching), we see the differences flip:
//! 
//! ```text
//! % cargo run --example throughput-benchmark --release
//!     Finished `release` profile [optimized] target(s) in 0.01s
//!      Running `target/release/examples/throughput-benchmark`
//! Benchmarking with:
//!   Writers: 5
//!   Readers: 5
//!   Messages per writer: 1000000
//!   Buffer capacity: 16384
//!   Batch size for market square: 1
//!   Total messages sent: 5000000
//! 
//! --- Market Square (Broadcast) ---
//! Time: 513.15375ms
//! Total reads: 25000000
//! Throughput: 48.72 million reads/sec
//! 
//! --- Crossbeam MPMC (Queue) ---
//! Time: 76.533708ms
//! Total reads: 5000000
//! Throughput: 65.33 million reads/sec
//! ```
//! 
//! There are other combinations of parameters that give vastly different performance profiles as well. For instance:
//! 
//! - The benchmarking example uses **spinning**. Being entirely lock-free and requiring at least one of the threads to be running `try_cleanup_old_slots` in a timely fashion, `market_square` *will* probabilistically grind to a halt when "reader + writer threads count > number of physical core threads" by a large margin and the simple cleanup strategy (utilized in the benchmark example) of just running `try_cleanup_old_slots` all the time on every read iteration is used.
//! 
//! - `market_square` scales exceptionally well with readers, but writers are bottlenecked by contention on the single atomic used for obtaining slots (one CAS instruction).
//! 
//! - As mentioned above, batching writes allow for massive performance improvements due to cache-locality and reducing contented CAS. Even with something as small as a batch size of 16, in the benchmark example above, market_square can tie with crossbeam in terms of total time elapsed overall.

pub mod map;
pub mod area;
pub mod storage;
pub mod cache_padded;
pub mod arithmetics;
