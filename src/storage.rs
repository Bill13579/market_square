use core::ptr::NonNull;

#[cfg(all(feature = "alloc", not(feature = "std")))]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std as alloc;

macro_rules! impl_copy_clone {
    ( $($name:ident),* ) => { // Matches one or more identifiers separated by commas
        $( // Starts a repetition for each identifier captured by `$name`
            impl<T> Copy for $name<T> { }

            impl<T> Clone for $name<T> {
                fn clone(&self) -> Self {
                    *self
                }
            }
        )* // Ends the repetition
    };
}

#[cfg(any(feature = "std", feature = "alloc"))]
impl_copy_clone!(BoxedStorage, BoxedSliceStorage);

impl_copy_clone!(StaticStorage, StaticSliceStorage);

/// A fixed memory region that outlives the data structure using it.
/// 
/// # Safety
/// - `as_ptr()` must return a stable pointer for the lifetime of the storage
/// - The memory must remain valid until `deallocate()` is called (or forever for static)
/// - The pointer must be properly aligned for `T`
pub unsafe trait FixedStorage<T> {
    /// Get a pointer to the storage. Must be stable (never moves).
    fn as_ptr(&self) -> NonNull<T>;

    /// Get a reference to the storage.
    unsafe fn as_ref(&self) -> &T {
        unsafe { self.as_ptr().as_ref() }
    }

    /// Deallocate the storage. No-op for static/arena storage.
    /// 
    /// # Safety
    /// - MUST ONLY BE CALLED ONCE
    /// - No references to the storage may exist after this call
    unsafe fn deallocate(&self);
}

pub unsafe trait FixedStorageMultiple<T>: FixedStorage<T> {
    /// Get the number of elements this storage can hold.
    fn capacity(&self) -> usize;

    /// Get a slice view of the storage.
    #[inline]
    fn slice(&self) -> &[T] {
        unsafe {
            core::slice::from_raw_parts(
                self.as_ptr().as_ptr(),
                self.capacity(),
            )
        }
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
use alloc::{boxed::Box, vec::Vec};

#[cfg(any(feature = "std", feature = "alloc"))]
/// Storage backed by a leaked `Box<T>`
#[repr(transparent)]
pub struct BoxedStorage<T> {
    ptr: NonNull<T>,
}

#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T: Send> Send for BoxedStorage<T> {}
#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T: Sync> Sync for BoxedStorage<T> {}

#[cfg(any(feature = "std", feature = "alloc"))]
impl<T> BoxedStorage<T> {
    /// Create storage by allocating a new T on the heap.
    #[inline]
    pub fn new(value: T) -> Self {
        Self::from_boxed(Box::new(value))
    }

    /// Create storage by leaking a boxed T.
    #[inline]
    pub fn from_boxed(boxed: Box<T>) -> Self {
        let ptr = Box::into_raw(boxed);
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
        }
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T> FixedStorage<T> for BoxedStorage<T> {
    fn as_ptr(&self) -> NonNull<T> {
        self.ptr
    }
    
    /// Deallocate the leaked Box.
    /// 
    /// # Safety
    /// - Must be called by the allocator which allocated the Box.
    unsafe fn deallocate(&self) {
        // Reconstruct the Box and drop it
        unsafe {
            let _ = Box::from_raw(self.ptr.as_ptr());
        }
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
/// Storage backed by a leaked `Box<[T]>`
pub struct BoxedSliceStorage<T> {
    ptr: NonNull<T>,
    capacity: usize,
}

#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T: Send> Send for BoxedSliceStorage<T> {}
#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T: Sync> Sync for BoxedSliceStorage<T> {}

#[cfg(any(feature = "std", feature = "alloc"))]
impl<T> BoxedSliceStorage<T> {
    /// Create storage by leaking a boxed slice.
    pub fn from_boxed_slice(boxed: Box<[T]>) -> Self {
        let capacity = boxed.len();
        let ptr = Box::into_raw(boxed) as *mut T;
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            capacity,
        }
    }
    
    /// Create storage with capacity, initializing each element.
    pub fn with_capacity_and_init<F>(capacity: usize, mut init: F) -> Self
    where
        F: FnMut() -> T,
    {
        let mut v = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            v.push(init());
        }
        Self::from_boxed_slice(v.into_boxed_slice())
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T> FixedStorage<T> for BoxedSliceStorage<T> {
    fn as_ptr(&self) -> NonNull<T> {
        self.ptr
    }

    /// Deallocate the leaked boxed slice.
    /// 
    /// # Safety
    /// - Must be called by the allocator which allocated the Boxed slice.
    unsafe fn deallocate(&self) {
        // Reconstruct the Box and drop it
        unsafe {
            let slice = core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity);
            let _ = Box::from_raw(slice as *mut [T]);
        }
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
unsafe impl<T> FixedStorageMultiple<T> for BoxedSliceStorage<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Storage backed by a static or externally-managed allocation.
/// Does NOT deallocate on drop.
#[repr(transparent)]
pub struct StaticStorage<T> {
    ptr: NonNull<T>,
    phantom: core::marker::PhantomData<T>,
}

unsafe impl<T: Send> Send for StaticStorage<T> {}
unsafe impl<T: Sync> Sync for StaticStorage<T> {}

impl<T> StaticStorage<T> {
    /// Wrap an externally-managed pointer.
    /// 
    /// # Safety
    /// - `ptr` must point to a valid `T`
    /// - The memory must remain valid for the lifetime of this storage
    /// - No other code may access the memory while this storage exists, or synchronization should be handled manually
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self {
            ptr,
            phantom: core::marker::PhantomData,
        }
    }
}

unsafe impl<T> FixedStorage<T> for StaticStorage<T> {
    fn as_ptr(&self) -> NonNull<T> {
        self.ptr
    }

    unsafe fn deallocate(&self) {
        // No-op: static/external memory is not our responsibility to free
    }
}

/// Storage backed by a static or externally-managed slice.
/// Does NOT deallocate on drop.
pub struct StaticSliceStorage<T> {
    ptr: NonNull<T>,
    capacity: usize,
    phantom: core::marker::PhantomData<T>,
}

unsafe impl<T: Send> Send for StaticSliceStorage<T> {}
unsafe impl<T: Sync> Sync for StaticSliceStorage<T> {}

impl<T> StaticSliceStorage<T> {
    /// Wrap a static mutable slice.
    /// 
    /// # Safety
    /// - The slice must live for the entire duration the storage is used
    /// - No other code may access the slice while this storage exists, or synchronization should be handled manually
    pub unsafe fn from_static(slice: &'static mut [T]) -> Self {
        Self {
            ptr: unsafe { NonNull::new_unchecked(slice.as_mut_ptr()) },
            capacity: slice.len(),
            phantom: core::marker::PhantomData,
        }
    }
    
    /// Wrap an externally-managed pointer.
    /// 
    /// # Safety
    /// - `ptr` must point to valid, aligned memory for `capacity` elements of `T`
    /// - The memory must remain valid for the lifetime of this storage
    /// - No other code may access the memory while this storage exists, or synchronization should be handled manually
    pub unsafe fn from_raw(ptr: NonNull<T>, capacity: usize) -> Self {
        Self {
            ptr,
            capacity,
            phantom: core::marker::PhantomData,
        }
    }
}

unsafe impl<T> FixedStorage<T> for StaticSliceStorage<T> {
    fn as_ptr(&self) -> NonNull<T> {
        self.ptr
    }

    unsafe fn deallocate(&self) {
        // No-op: static/external memory is not our responsibility to free
    }
}

unsafe impl<T> FixedStorageMultiple<T> for StaticSliceStorage<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
}
