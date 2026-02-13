**Feb 12, 2026:** The current `main` branch contains now an implementation generic over the underlying memory space used. **`no_alloc` is thus now supported!**

[<img src="https://kooworks.org/xathek" alt="Xathek" width="280">](https://kooworks.org/xathek)

# market_square

[![Crates.io](https://img.shields.io/crates/v/market_square)](https://crates.io/crates/market_square)
[![Docs.rs](https://img.shields.io/docsrs/market_square)](https://docs.rs/market_square)

This crate provides a lock-free, bounded, multi-producer and multi-consumer "square", where readers and writers can join and leave at any time. It is also built to be able to do so fast.

To achieve both the flexibility and the speed, there are some custom lock-free algorithms used. See [this post](./ALG.md) for more details!

## The market square

In a bustling market square, merchants shout openly at anyone nearby about their goods, hoping to catch some sales; and even in such a noisy and energetic place, buyers would then parse all of the things they hear, and get just the things they came there for; although, it's also possible they might find something they forgot they needed, and grab those as well! When it's cheap to do so, having threads being able to read even unrelated messages can create flexibility for the future, for this exact reason.

The goal is to emulate the bustle of the market square. Imagine if you are an `AreaReader`. You walk into a market square where many people are talking (`AreaWriter`s). When you enter the area, you are guaranteed to hear every single message produced since you've entered into that area, which is the hearing range. That message will then be guaranteed to stay alive until such a time when you and all other people who have heard that message decides to forget it.

## Features

The goal of this crate is high-performance broadcast, but not at the expense of ergonomics *or* power. Here are some notable features:

- Lock-free
- Dynamic enter/exit of readers and writers
- Readers-are-also-writers, and new readers are dirt cheap to create
- Reader suspension so that readers who are momentarily away can avoid holding up the cleaning of old messages
- Batched publishing for writers, and batched read for readers
- In-place message writing through `MaybeUninit`; you can skip intermediate struct allocation
- Any thread can clean-up old messages at any time, up to user in terms of where and when clean-up is done
- CPU pre-fetching and cache-locality friendly structures
- Ergonomic Rust-y RAII based API for ease-of-use
- Maximum control and customization options with low-level control; bring-your-own-locks!
- `no_std` **and `no_alloc` (in main branch)** support

## Usage

Add the following to your `Cargo.toml` file:

```toml
# with std
[dependencies]
market_square = "0.1"

# with no_std
[dependencies]
market_square = { version = "0.1", default-features = false, features = ["alloc"] }

# with no_alloc (see examples/no-alloc-static.rs for an example using static memory)
[dependencies]
market_square = { version = "0.1", default-features = false }
```

Usage is simple; you can create a new Area like so:

```rust
use market_square::area::area;

let (writer, mut reader) = area(buffer_capacity, reader_capacity);

// `buffer_capacity` is the message ring buffer's capacity, and `reader_capacity` is the maximum number of readers at any given moment, which needs to be a power-of-two.
```

Use `reader.create_reader()` to create more readers, and `writer.create_writer()` to create more writers (creating new readers is fallible if the reader capacity is exceeded).

For clean-up, **`try_cleanup_old_slots()` must be used periodically *somewhere*!!** The power of deciding where to clean-up is entirely in your hands. You can do it inside of the reader threads (which is recommended), or have a completely separate thread that does it (this can cause things to get stuck if you use spin-loops for getting writer slots and the clean-up thread is not scheduled by the OS), or in the writer threads if you wanted to (there's a caveat to this which is noted in both examples). **But you have to make sure it runs *somewhere*, because otherwise you will quickly run out of buffer slots for new messages.**

Suspend readers with `suspend()`, and they will no longer maintain a claim to old messages and prevent clean-up.

**See the [hello world](./examples/hello-world.rs) and [readers-are-also-writers](./examples/readers-are-also-writers.rs) examples for a full overview.** There is also a more complex benchmarking script example there which uses batching.

## Benchmarks

A simple throughput benchmark is available under `examples`, here is an example run on an M1 MacBook Air:

```
% cargo run --example throughput-benchmark --release
    Finished `release` profile [optimized] target(s) in 0.01s
     Running `target/release/examples/throughput-benchmark`
Benchmarking with:
  Writers: 5
  Readers: 5
  Messages per writer: 1000000
  Buffer capacity: 16384
  Batch size for market square: 64
  Total messages sent: 5000000

--- Market Square (Broadcast) ---
Time: 25.910375ms
Total reads: 25000000
Throughput: 964.86 million reads/sec

--- Crossbeam MPMC (Queue) ---
Time: 85.779833ms
Total reads: 5000000
Throughput: 58.29 million reads/sec
```

Why does market_square seem faster than crossbeam? ***Because they do different things!!***

`crossbeam-channel` is a work queue, and so each sender's message is only given to one of the receivers.

`market_square` is a broadcast system, and so each writer's message is shown to every single reader.

This is why "Total reads" here is higher, since there are, in the case of the example above, for example, technically five times as many reads, where all 5 readers have to have a look at the messages and pass on them before Drop is allowed on those messages.

Being purely broadcasting also provides `market_square` with tricks that a work queue like `crossbeam-channel` might not be able to adopt. For instance, the example above uses batching, where the sender grabs 64 slots at once and publishes them. If we reduce batching to 1 (so, no batching), we see the differences flip:

```
% cargo run --example throughput-benchmark --release
    Finished `release` profile [optimized] target(s) in 0.01s
     Running `target/release/examples/throughput-benchmark`
Benchmarking with:
  Writers: 5
  Readers: 5
  Messages per writer: 1000000
  Buffer capacity: 16384
  Batch size for market square: 1
  Total messages sent: 5000000

--- Market Square (Broadcast) ---
Time: 513.15375ms
Total reads: 25000000
Throughput: 48.72 million reads/sec

--- Crossbeam MPMC (Queue) ---
Time: 76.533708ms
Total reads: 5000000
Throughput: 65.33 million reads/sec
```

There are other combinations of parameters that give vastly different performance profiles as well. For instance:

- The benchmarking example uses **spinning**. Being entirely lock-free and requiring at least one of the threads to be running `try_cleanup_old_slots` in a timely fashion, `market_square` *will* probabilistically grind to a halt when "reader + writer threads count > number of physical core threads" by a large margin and the simple cleanup strategy (utilized in the benchmark example) of just running `try_cleanup_old_slots` all the time on every read iteration is used.

- `market_square` scales exceptionally well with readers, but writers are bottlenecked by contention on the single atomic used for obtaining slots (one CAS instruction).

- As mentioned above, batching writes allow for massive performance improvements due to cache-locality and reducing contented CAS. Even with something as small as a batch size of 16, in the benchmark example above, market_square can tie with crossbeam in terms of total time elapsed overall.
