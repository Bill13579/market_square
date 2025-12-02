use std::thread;
use market_square::area::area;

fn main() {
    let buffer_capacity = 20; // This is the ring buffer capacity; messages are stored in it.
    let reader_capacity = 16; // This is the maximum number of readers at any given moment. Needs to be a power-of-two.

    let (writer, mut reader) = area(buffer_capacity, reader_capacity);

    reader.suspend().unwrap(); // You can immediately suspend the first reader if you wish. Since all readers must keep reading and moving forward to not hold up clean-up unless they are suspended, suspending the first reader lets you do the usual pattern of cloning from a "template" N times.

    let mut reader_handles = vec![];
    let mut writer_handles = vec![];

    // Reader threads
    for i in 0..10 {
        let mut reader = reader.create_reader().unwrap();

        reader_handles.push(thread::spawn(move || {
            while let Ok(slice) = reader.read_with_check() {
                let _ = slice.try_cleanup_old_slots(); // Readers calling clean-up tends to work well.
                if slice.len() != 0 {
                    println!(
                        "\n[reader {}] got {} messages! {:?}",
                        i,
                        slice.len() as u64,
                        slice.iter().cloned().collect::<Vec<String>>()
                    );
                }
            }
        }));
    }

    // Writer threads
    for i in 0..10 {
        let writer = writer.create_writer();

        writer_handles.push(thread::spawn(move || {
            for msg_idx in 0..8 {
                loop {
                    // Writers should be careful not to begin cleanup before any readers can join. Here, we just don't clean up from the writer-side.
                    // let _ = writer.try_cleanup_old_slots();

                    match writer.reserve(1) {
                        Ok(mut reservation) => {
                            reservation.get_mut(0).unwrap().write(format!("hello from writer {} ({})", i, msg_idx));
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

    drop(reader);
    drop(writer);
    for handle in writer_handles {
        handle.join().unwrap();
    }
    for handle in reader_handles {
        handle.join().unwrap();
    }

    println!("Done");
}