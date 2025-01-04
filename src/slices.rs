use core::{
    borrow::{Borrow, BorrowMut},
    iter::IntoIterator,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr,
    ops::{Deref, DerefMut},
};

pub trait TrReclaim<T>: Sized {
    fn reclaim(&mut self, t: &mut T);
}

/// Relies on the unstable feature `#![feature(min_specialization)]`
trait CloneFromSpec<T> {
    fn spec_clone_from(&mut self, src: &[T]);
}

/// The rented slice from the rx of the
/// [RingBuffer](crate::ring_buffer::RingBuffer).
pub struct SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    slice_: B,
    reclaim_: Option<R>,
    _mark_t_: PhantomData<[T]>,
}

impl<B, T, R> SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    pub(super) const fn new(slice: B, reclaim: Option<R>) -> Self {
        SliceRef {
            slice_: slice,
            reclaim_: reclaim,
            _mark_t_: PhantomData,
        }
    }
}

impl<B, T, R> Borrow<[T]> for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    fn borrow(&self) -> &[T] {
        self.slice_.borrow()
    }
}

impl<B, T, R> Drop for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    fn drop(&mut self) {
        let Option::Some(mut r) = self.reclaim_.take() else {
            return;
        };
        r.reclaim(self)
    }
}

impl<B, T, R> Deref for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.slice_.borrow()
    }
}

impl<B, T, R> IntoIterator for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: TrReclaim<Self>,
{
    type Item = T;
    type IntoIter = Iter<Self, Self::Item>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        unsafe { Iter::new_unchecked(self) }
    }
}

/// The rented slice for tx of the [RingBuffer](crate::ring_buffer::RingBuffer)
pub struct SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    slice_mut_: B,
    reclaim_: Option<R>,
    _mark_t_: PhantomData<[T]>,
}

impl<B, T, R> SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    pub const fn new(slice_mut: B, reclaim: Option<R>) -> Self {
        SliceMut {
            slice_mut_: slice_mut,
            reclaim_: reclaim,
            _mark_t_: PhantomData,
        }
    }
}

impl<B, T, R> Borrow<[MaybeUninit<T>]> for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    #[inline]
    fn borrow(&self) -> &[MaybeUninit<T>] {
        self.slice_mut_.borrow()
    }
}

impl<B, T, R> BorrowMut<[MaybeUninit<T>]> for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    #[inline]
    fn borrow_mut(&mut self) -> &mut [MaybeUninit<T>] {
        self.slice_mut_.borrow_mut()
    }
}

impl<B, T, R> Drop for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    fn drop(&mut self) {
        let Option::Some(mut r) = self.reclaim_.take() else {
            return;
        };
        r.reclaim(self)
    }
}

impl<B, T, R> Deref for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    type Target = [MaybeUninit<T>];

    fn deref(&self) -> &[MaybeUninit<T>] {
        self.slice_mut_.borrow()
    }
}

impl<B, T, R> DerefMut for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
{
    fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
        self.slice_mut_.borrow_mut()
    }
}

impl<B, T, R> IntoIterator for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: TrReclaim<Self>,
    T: Unpin,
{
    type Item = MaybeUninit<T>;
    type IntoIter = Iter<Self, Self::Item>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        unsafe { Iter::new_unchecked(self) }
    }
}

impl<B, T, R> SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    T: Clone,
    R: TrReclaim<Self>,
{
    /// Call [clone_from_slice](Self::clone_from_slice) when `T` not [`Copy`]
    /// but [`Clone`], or [copy_from_slice](Self::copy_from_slice) only when
    /// `T` is [`Copy`].
    pub fn clone_or_copy(&mut self, src: &[T]) {
        CloneFromSpec::spec_clone_from(self, src);
    }

    /// Overwrite elements in the slice cloning from source without dropping.
    pub fn clone_from_slice(&mut self, src: &[T]) {
        assert!(
            self.len() == src.len(),
            "destination and source slices have different lengths",
        );
        let len = self.len();
        let src = &src[..len];
        for i in 0..len {
            self[i].write(src[i].clone());
        }
    }
}

impl<B, T, R> SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    T: Copy,
    R: TrReclaim<Self>,
{
    /// A convenient wrapper around [copy_from_slice](<[T]>::copy_from_slice)
    pub fn copy_from_slice(&mut self, src: &[T]) {
        let slice = unsafe {
            let p = self.deref_mut() as *mut [MaybeUninit<T>] as *mut [T];
            &mut *p
        };
        slice.copy_from_slice(src);
    }
}

/// The iterator for [SliceRef](crate::slices::SliceRef)
pub struct Iter<S, T>
where
    S: Deref<Target = [T]>,
{
    slice_: MaybeUninit<S>,
    offset_: usize,
}

impl<S, T> Iter<S, T>
where
    S: Deref<Target = [T]>,
{
    /// The iterator for `SliceRef` and `SliceMut`. 
    /// 
    /// # Safety
    /// 
    /// * `slice` must be the semantic owner of the [T] (like [SliceRef]);
    /// * All elements of the `slice` must be safe to move.
    pub const unsafe fn new_unchecked(slice: S) -> Self {
        Iter {
            slice_: MaybeUninit::new(slice),
            offset_: 0,
        }
    }
}

impl<S, T> Iter<S, T>
where
    S: Deref<Target = [T]>,
    T: Clone,
{
    /// The iterator for `SliceRef` and `SliceMut`. 
    pub const fn new(slice: S) -> Self {
        Iter {
            slice_: MaybeUninit::new(slice),
            offset_: 0,
        }
    }
}

impl<S, T> Iterator for Iter<S, T>
where
    S: Deref<Target = [T]>,
{
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let slice_ref = unsafe { self.slice_.assume_init_ref() };
        let curr_offset: usize = self.offset_;
        if curr_offset < slice_ref.len() {
            let item = &slice_ref[self.offset_];
            self.offset_ += 1;
            Option::Some(
                // Safe here because we will never read the item again, and T
                // is `Unpin`, so it is safe to move.
                unsafe { ptr::read(item) }
            )
        } else {
            Option::None
        }
    }
}

impl<S, T> Drop for Iter<S, T>
where
    S: Deref<Target = [T]>,
{
    fn drop(&mut self) {
        unsafe { self.slice_.assume_init_drop() }; 
    }
}

/// Default clone behaviour
impl<B, T, R> CloneFromSpec<T> for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    T: Clone,
    R: TrReclaim<Self>,
{
    default fn spec_clone_from(&mut self, src: &[T]) {
        self.clone_from_slice(src);
    }
}

/// Specialized clone behaviour when T: Copy
impl<B, T, R> CloneFromSpec<T> for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    T: Copy,
    R: TrReclaim<Self>,
{
    fn spec_clone_from(&mut self, src: &[T]) {
        self.copy_from_slice(src);
    }
}
