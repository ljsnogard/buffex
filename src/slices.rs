use core::{
    borrow::{Borrow, BorrowMut},
    iter::IntoIterator,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr,
    ops::{Deref, DerefMut},
};

pub struct SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: FnOnce(&mut Self),
{
    slice_: B,
    reclaim_: Option<R>,
    _mark_t_: PhantomData<[T]>,
}

impl<B, T, R> SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: FnOnce(&mut Self),
{
    pub const fn new(slice: B, reclaim: Option<R>) -> Self {
        SliceRef {
            slice_: slice,
            reclaim_: reclaim,
            _mark_t_: PhantomData,
        }
    }
}

impl<B, T, R> Drop for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: FnOnce(&mut Self),
{
    fn drop(&mut self) {
        let Option::Some(r) = self.reclaim_.take() else {
            return;
        };
        r(self)
    }
}

impl<B, T, R> Deref for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: FnOnce(&mut Self),
{
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.slice_.borrow()
    }
}

impl<B, T, R> IntoIterator for SliceRef<B, T, R>
where
    B: Borrow<[T]>,
    R: FnOnce(&mut Self),
    T: Unpin,
{
    type Item = T;
    type IntoIter = Iter<Self, Self::Item>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        unsafe { Iter::new_unchecked(self) }
    }
}

pub struct SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
{
    slice_mut_: B,
    reclaim_: Option<R>,
    _mark_t_: PhantomData<[T]>,
}

impl<B, T, R> SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
{
    pub const fn new(slice_mut: B, reclaim: Option<R>) -> Self {
        SliceMut {
            slice_mut_: slice_mut,
            reclaim_: reclaim,
            _mark_t_: PhantomData,
        }
    }
}

impl<B, T, R> Drop for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
{
    fn drop(&mut self) {
        let Option::Some(r) = self.reclaim_.take() else {
            return;
        };
        r(self)
    }
}

impl<B, T, R> Deref for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
{
    type Target = [MaybeUninit<T>];

    fn deref(&self) -> &[MaybeUninit<T>] {
        self.slice_mut_.borrow()
    }
}

impl<B, T, R> DerefMut for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
{
    fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
        self.slice_mut_.borrow_mut()
    }
}

impl<B, T, R> IntoIterator for SliceMut<B, T, R>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    R: FnOnce(&mut Self),
    T: Unpin,
{
    type Item = MaybeUninit<T>;
    type IntoIter = Iter<Self, Self::Item>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        unsafe { Iter::new_unchecked(self) }
    }
}

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
    /// * `slice` must be the owner of the slice which it can dereference.
    /// * Elements of the `slice` must be safe to move.
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
