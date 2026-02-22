use core::{
    marker::PhantomData,
};

use crate::{area::{AreaWriterTrait, PublishError, Reservation}, arithmetics::{NumericType, gen_add_msb_masked, gen_dist_msb_masked, gen_gte_msb_masked}};

/// A single slot in a [`ReservationIter`] that must be written to before being dropped.
///
/// Panics on drop if [`write()`](ReservationSlot::write) was not called.
pub struct ReservationSlot<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    writer: &'a W,
    generation: NumericType,
    written_count: &'a mut usize,
    written: bool,
    phantom: PhantomData<T>,
}

impl<'a, W, T> ReservationSlot<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    /// Write a value into this slot. Returns a mutable reference to the written value.
    ///
    /// # Panics
    /// Panics if called more than once on the same slot.
    pub fn write(&mut self, val: T) -> &mut T {
        assert!(!self.written, "ReservationSlot::write called more than once!");
        unsafe {
            let ptr = self.writer.get_slot_ptr(self.generation);
            ptr.write(val);
            self.written = true;
            *self.written_count += 1;
            &mut *ptr
        }
    }

    /// Get the generation number for this slot.
    pub fn generation(&self) -> NumericType {
        self.generation
    }

    /// Check if this slot has been written to.
    pub fn is_written(&self) -> bool {
        self.written
    }
}

impl<'a, W, T> Drop for ReservationSlot<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    fn drop(&mut self) {
        if !self.written {
            panic!(
                "ReservationSlot at generation {} dropped without calling write()! \
                 All yielded slots must be written to.",
                self.generation
            );
        }
    }
}

/// An iterator over reserved slots that ensures safe initialization before publishing.
///
/// Created by calling `.into_iter()` on a [`Reservation`]. Each yielded
/// [`ReservationSlot`] must have [`write()`](ReservationSlot::write) called on it
/// before it is dropped. Once all slots are written, [`publish()`](ReservationIter::publish)
/// can be called safely (it is not marked `unsafe`, unlike [`publish()`](Reservation::publish) on [`Reservation`]).
///
/// # Example
/// ```ignore
/// let reservation = writer.reserve::<()>(4)?;
/// let mut iter = reservation.into_iter();
/// for mut slot in &mut iter {
///     slot.write(42u64);
/// }
/// let _ = iter.publish::<()>();
/// ```
pub struct ReservationIter<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    writer: &'a W,
    start_gen: NumericType,
    end_gen: NumericType,
    cursor: NumericType,
    written_count: usize,
    total: usize,
    published: bool,
    phantom: PhantomData<T>,
}

impl<'a, W, T> ReservationIter<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    /// Get the total number of slots in this reservation.
    pub fn len(&self) -> usize {
        self.total
    }

    /// Check if the reservation is empty.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Get the number of slots that have been written to so far.
    pub fn written_count(&self) -> usize {
        self.written_count
    }

    /// Check if all slots have been written to.
    pub fn all_written(&self) -> bool {
        self.written_count == self.total
    }

    /// Get the number of remaining slots to be yielded.
    pub fn remaining(&self) -> usize {
        gen_dist_msb_masked(self.end_gen, self.cursor) as usize
    }

    /// Publish all written slots.
    ///
    /// This is the safe counterpart to [`Reservation::publish()`], which is `unsafe`.
    /// Because the iterator enforces that every yielded slot is written to, all slots
    /// are guaranteed to be initialized when this is called.
    ///
    /// # Panics
    /// Panics if not all slots have been written to.
    ///
    /// Use `E = ()` for CAS-based publishing (multiple writers) or
    /// `E = Exclusive` for direct store (if you can provide single writer guarantee).
    pub fn publish<E>(mut self) -> Result<(), (Self, PublishError)> {
        assert!(
            self.all_written(),
            "Cannot publish ReservationIter: only {}/{} slots written. \
             All slots must be initialized before publishing.",
            self.written_count,
            self.total
        );

        self.published = true;
        match self.writer.publish_slots::<E>(self.start_gen, self.end_gen) {
            Ok(_) => Ok(()),
            Err(err) => {
                self.published = false;
                Err((self, err))
            }
        }
    }

    /// Publish all written slots, spinning on CAS failure until success.
    ///
    /// # Panics
    /// Panics if not all slots have been written to.
    pub fn publish_spin(self) {
        let mut iter = self;

        #[cfg(debug_assertions)]
        let mut tr = 0;
        loop {
            match iter.publish::<()>() {
                Ok(_) => return,
                Err((returned, err)) => {
                    #[cfg(debug_assertions)]
                    {
                        use crate::area::SPIN_LIMIT;

                        tr += 1;
                        if tr >= SPIN_LIMIT {
                            panic!("SPIN_LIMIT reached! (ReservationIter::publish_spin)");
                        }
                    }

                    iter = returned;
                    core::hint::spin_loop();

                    match err {
                        PublishError::CasFailed => continue,
                        PublishError::OutOfOrderPublishAttempt => {
                            unreachable!("OutOfOrderPublishAttempt should never happen in publish_spin since the range check is implicit in the CAS failure. There are no separate checks for this since there's no clear way to distinguish an out-of-order publish attempt from normal publishing failure due to correct coordination with other writers.")
                        },
                    }
                }
            }
        }
    }
}

impl<'a, W, T> Iterator for ReservationIter<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    type Item = ReservationSlot<'a, W, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if gen_gte_msb_masked(self.cursor, self.end_gen) {
            return None;
        }

        let generation = self.cursor;
        self.cursor = gen_add_msb_masked(self.cursor, 1);

        Some(ReservationSlot {
            writer: self.writer,
            generation,
            // SAFETY: We hold &mut self so no other slot can exist simultaneously.
            // The borrow checker sees 'short (the call's lifetime) here, but the slot borrows &mut self.written_count
            // which lives for 'a. We ensure only one slot exists at a time by requiring
            // the previous slot to be dropped before next() yields another (implicitly through taking &mut self), so this is sound. There is no aliasing. `miri-test-2` covers this.
            written_count: unsafe { &mut *(&raw mut self.written_count) },
            written: false,
            phantom: PhantomData,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining();
        (remaining, Some(remaining))
    }
}

impl<'a, W, T> ExactSizeIterator for ReservationIter<'a, W, T>
where
    W: AreaWriterTrait<T>,
{}

impl<'a, W, T> Drop for ReservationIter<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    fn drop(&mut self) {
        if !self.published {
            panic!(
                "ReservationIter dropped without publishing! \
                 Slots {}..{} are leaked/blocked ({}/{} written).",
                self.start_gen, self.end_gen, self.written_count, self.total
            );
        }
    }
}

impl<'a, W, T> IntoIterator for Reservation<'a, W, T>
where
    W: AreaWriterTrait<T>,
{
    type Item = ReservationSlot<'a, W, T>;
    type IntoIter = ReservationIter<'a, W, T>;

    fn into_iter(self) -> Self::IntoIter {
        let start_gen = self.start_gen;
        let end_gen = self.end_gen;
        let total = self.len();

        // Mark the reservation as published so it doesn't panic on drop.
        // Ownership transfers to the ReservationIter.
        let mut reservation = self;
        reservation.published = true;
        // Drop the reservation (it's marked published so it won't panic)
        let writer_ref = reservation.writer;
        core::mem::drop(reservation);

        ReservationIter {
            writer: writer_ref,
            start_gen,
            end_gen,
            cursor: start_gen,
            written_count: 0,
            total,
            published: false,
            phantom: PhantomData,
        }
    }
}

#[cfg(test)]
#[cfg(any(feature = "std", feature = "alloc"))]
mod tests {
    use crate::area::area;

    use super::*;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    extern crate alloc;
    #[cfg(feature = "std")]
    extern crate std as alloc;

    #[cfg(any(feature = "std", feature = "alloc"))]
    use alloc::vec::Vec;

    #[test]
    fn test_reservation_iter_basic() {
        let (writer, mut reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(4).unwrap();
        let mut iter = reservation.into_iter();

        let mut i = 0u64;
        while let Some(mut slot) = iter.next() {
            slot.write(i * 100);
            i += 1;
        }

        assert_eq!(iter.written_count(), 4);
        assert!(iter.all_written());
        iter.publish_spin();

        let slice = reader.read();
        assert_eq!(slice.len(), 4);
        let values: Vec<u64> = slice.iter().copied().collect();
        assert_eq!(values, alloc::vec![0, 100, 200, 300]);
    }

    #[test]
    fn test_reservation_iter_for_loop() {
        let (writer, mut reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(3).unwrap();
        let mut iter = reservation.into_iter();
        let mut val = 10u64;

        for mut slot in &mut iter {
            slot.write(val);
            val += 10;
        }

        iter.publish_spin();

        let slice = reader.read();
        assert_eq!(slice.len(), 3);
        assert_eq!(*slice.get(0).unwrap(), 10);
        assert_eq!(*slice.get(1).unwrap(), 20);
        assert_eq!(*slice.get(2).unwrap(), 30);
    }

    #[test]
    #[should_panic(expected = "ReservationSlot at generation")]
    fn test_reservation_iter_slot_not_written() {
        let (writer, _reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(4).unwrap();
        let mut iter = reservation.into_iter();
        let _slot = iter.next().unwrap();
        iter.published = true; // Prevent double-panic from breaking should_panic
        // Drop slot without writing should panic
    }

    #[test]
    #[should_panic(expected = "Cannot publish ReservationIter")]
    fn test_reservation_iter_publish_before_all_written() {
        let (writer, _reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(4).unwrap();
        let mut iter = reservation.into_iter();

        iter.next().unwrap().write(1);
        iter.next().unwrap().write(2);

        iter.published = true; // Prevent double-panic from breaking should_panic
        // Trying to publish with only 2/4 written should panic
        let _ = iter.publish::<()>();
    }

    #[test]
    #[should_panic(expected = "ReservationIter dropped without publishing")]
    fn test_reservation_iter_drop_without_publish() {
        let (writer, _reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(2).unwrap();
        let mut iter = reservation.into_iter();
        iter.next().unwrap().write(1);
        iter.next().unwrap().write(2);

        core::mem::drop(iter);
        // Drop without publish should panic
    }

    #[test]
    fn test_reservation_iter_empty() {
        let (writer, _reader) = area::<u64>(16, 8);

        let reservation = writer.reserve_best_effort::<()>(0).unwrap_or_else(|_| {
            panic!("unexpected");
        });
        let iter = reservation.into_iter();
        assert!(iter.is_empty());
        assert!(iter.all_written());
        iter.publish_spin();
    }

    #[test]
    fn test_reservation_iter_best_effort_empty() {
        let (writer, _reader) = area::<u64>(4, 8);

        // Fill buffer
        let (start, end) = writer.try_reserve_slots::<()>(4).unwrap();
        unsafe {
            for g in start..end {
                writer.get_slot_ptr(g).write(g as u64);
            }
        }
        writer.publish_slots::<()>(start, end).unwrap();

        // Best effort with full buffer should lead to an empty reservation
        let reservation = writer.reserve_best_effort::<()>(4).unwrap();
        assert!(reservation.is_empty());
        let iter = reservation.into_iter();
        assert!(iter.all_written());
        iter.publish_spin();
    }

    #[test]
    fn test_reservation_iter_exact_size() {
        let (writer, _reader) = area::<u64>(16, 8);

        let reservation = writer.reserve::<()>(5).unwrap();
        let mut iter = reservation.into_iter();
        assert_eq!(iter.len(), 5);

        iter.next().unwrap().write(0);
        assert_eq!(iter.remaining(), 4);

        iter.next().unwrap().write(0);
        iter.next().unwrap().write(0);
        iter.next().unwrap().write(0);
        iter.next().unwrap().write(0);
        assert_eq!(iter.remaining(), 0);

        iter.publish_spin();
    }
}
