use core::{
    borrow::BorrowMut,
    mem::MaybeUninit,
};

use atomex::TrCmpxchOrderings;

use crate::slices::{SliceMut, SliceRef};
use super::buffer_::RingBuffer;

pub type ReclSliceRef<'a, P, T, O> =
    SliceRef<&'a [T], T, ReaderForwardFn<'a, P, T, O>>;

pub type ReclSliceMut<'a, P, T, O> =
    SliceMut<&'a mut [MaybeUninit<T>], T, WriterForwardFn<'a, P, T, O>>;

/// A wrapper around the internal function that forwards the reader position,
/// and will be invoked when a `ReclSliceRef` drops.
pub struct ReaderForwardFn<'a, P, T, O>(&'a RingBuffer<P, T, O>)
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

impl<'a, P, T, O> ReaderForwardFn<'a, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub const fn new(ring_buff: &'a RingBuffer<P, T, O>) -> Self {
        ReaderForwardFn(ring_buff)
    }
}

impl<'a, P, T, O> FnOnce<(&mut ReclSliceRef<'a, P, T, O>,)> for ReaderForwardFn<'a, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type Output = ();

    extern "rust-call" fn call_once(
        self,
        args: (&mut ReclSliceRef<'a, P, T, O>,),
    ) -> Self::Output {
        let slice_ref = args.0;
        let x = self.0.state().rx_forward(slice_ref.len());
        assert!(x.is_ok())
    }
}

/// A wrapper around the internal function that forwards the writer position,
/// and will be invoked when a `ReclSliceMut` drops.
pub struct WriterForwardFn<'a, P, T, O>(&'a RingBuffer<P, T, O>)
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

impl<'a, P, T, O> WriterForwardFn<'a, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub const fn new(ring_buff: &'a RingBuffer<P, T, O>) -> Self {
        WriterForwardFn(ring_buff)
    }
}

impl<'a, P, T, O> FnOnce<(&mut ReclSliceMut<'a, P, T, O>,)> for WriterForwardFn<'a, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type Output = ();

    extern "rust-call" fn call_once(
        self,
        args: (&mut ReclSliceMut<'a, P, T, O>,),
    ) -> Self::Output {
        let slice_mut = args.0;
        let x = self.0.state().tx_forward(slice_mut.len());
        assert!(x.is_ok())
    }
}
