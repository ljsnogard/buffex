use core::borrow::BorrowMut;

use asyncex_channel::x_deps::atomex;
use atomex::TrCmpxchOrderings;

use crate::slices::{SliceMut, SliceRef};
use super::buffer_::RingBuffer;

pub type ReclSliceRef<'a, P, T, O> = SliceRef<&'a [T], T, ReaderForwardFn<'a, P, T, O>>;
pub type ReclSliceMut<'a, P, T, O> = SliceMut<&'a mut [T], T, WriterForwardFn<'a, P, T, O>>;

pub struct ReaderForwardFn<'a, P, T, O>(&'a RingBuffer<P, T, O>)
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, P, T, O> ReaderForwardFn<'a, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub const fn new(ring_buff: &'a RingBuffer<P, T, O>) -> Self {
        ReaderForwardFn(ring_buff)
    }
}

impl<'a, P, T, O> FnOnce<(&mut ReclSliceRef<'a, P, T, O>,)> for ReaderForwardFn<'a, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = ();

    extern "rust-call" fn call_once(
        self,
        args: (&mut ReclSliceRef<'a, P, T, O>,),
    ) -> Self::Output {
        let slice_ref = args.0;
        let x = self.0.state().reader_forward(slice_ref.len());
        assert!(x.is_ok())
    }
}

pub struct WriterForwardFn<'a, P, T, O>(&'a RingBuffer<P, T, O>)
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, P, T, O> WriterForwardFn<'a, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub const fn new(ring_buff: &'a RingBuffer<P, T, O>) -> Self {
        WriterForwardFn(ring_buff)
    }
}

impl<'a, P, T, O> FnOnce<(&mut ReclSliceMut<'a, P, T, O>,)> for WriterForwardFn<'a, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = ();

    extern "rust-call" fn call_once(
        self,
        args: (&mut ReclSliceMut<'a, P, T, O>,),
    ) -> Self::Output {
        let slice_ref = args.0;
        let x = self.0.state().writer_forward(slice_ref.len());
        assert!(x.is_ok())
    }
}
