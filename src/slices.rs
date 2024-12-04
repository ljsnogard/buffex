use core::{
    borrow::{Borrow, BorrowMut},
    marker::PhantomData,
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

pub struct SliceMut<B, T, R>
where
    B: BorrowMut<[T]>,
    R: FnOnce(&mut Self),
{
    slice_mut_: B,
    reclaim_: Option<R>,
    _mark_t_: PhantomData<[T]>,
}

impl<B, T, R> SliceMut<B, T, R>
where
    B: BorrowMut<[T]>,
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
    B: BorrowMut<[T]>,
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
    B: BorrowMut<[T]>,
    R: FnOnce(&mut Self),
{
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.slice_mut_.borrow()
    }
}

impl<B, T, R> DerefMut for SliceMut<B, T, R>
where
    B: BorrowMut<[T]>,
    R: FnOnce(&mut Self),
{
    fn deref_mut(&mut self) -> &mut [T] {
        self.slice_mut_.borrow_mut()
    }
}
