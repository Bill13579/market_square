use std::thread;
use market_square::area::area;

// So that all writing starts together.
use std::sync::{Arc, Barrier};

fn main() {
    let buffer_capacity = 20; // This is the ring buffer capacity; messages are stored in it.
    let reader_capacity = 32; // This is the maximum number of participants at any given moment. Needs to be a power-of-two.

    let (_, mut area) = area(buffer_capacity, reader_capacity);

    area.suspend().unwrap(); // See the "hello-world.rs" example for explanation

    let mut thread_handles = vec![];

    // Spawn threads
    let barrier = Arc::new(Barrier::new(20));
    for i in 0..20 {
        let mut area = area.create_reader().unwrap();

        let expected_msgs = 20 * 8;

        let barrier = barrier.clone();
        thread_handles.push(thread::spawn(move || {
            let mut msg_idx = 0;
            let mut msgs_heard_count = 0;
            barrier.wait();
            loop {
                if msg_idx < 8 {
                    match area.reserve(1) {
                        Ok(mut reservation) => {
                            reservation.get_mut(0).unwrap().write(format!("hello from thread {} ({})", i, msg_idx));
                            unsafe { reservation.publish_spin() };
                            msg_idx += 1;
                        }
                        Err(_) => {
                            thread::yield_now();
                        }
                    }
                }

                // Here, we will hear our own messages. Since we are the ones that wrote the messages, 
                // they are already in our local cache-line, and so reading it back is just like reading 
                // a local variable, meaning checking it adds close to zero overhead.
                // This was intentionally designed this way because of the idea of allowing for more 
                // communication whenever possible and when it is cheap to do so.
                // 
                // Also, when you are shouting in a market square, you always hear what you say yourself!
                let slice = area.read();
                let _ = slice.try_cleanup_old_slots();
                if slice.len() != 0 {
                    println!(
                        "\n[thread {}] got {} messages! {:?}",
                        i,
                        slice.len() as u64,
                        slice.iter().cloned().collect::<Vec<String>>()
                    );
                    msgs_heard_count += slice.len();
                }

                if msgs_heard_count == expected_msgs {
                    break;
                }
            }
        }));
    }

    drop(area);
    for handle in thread_handles {
        handle.join().unwrap();
    }

    println!("Done");
}