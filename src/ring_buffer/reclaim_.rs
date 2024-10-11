use core::borrow::BorrowMut;
use abs_buff::{
    IoSliceMut, IoSliceRef, TrReclaimIoSliceRef, TrReclaimIoSliceMut,
};
use atomex::{
    x_deps::funty,
    TrAtomicData, TrCmpxchOrderings,
};
use asyncex::x_deps::atomex;

use super::buffer_::RingBuffer;

pub struct Reclaim<'a, P, T, D, O>(&'a RingBuffer<P, T, D, O>)
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings;

impl<'a, P, T, D, O> Reclaim<'a, P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(ring_buffer: &'a RingBuffer<P, T, D, O>) -> Self {
        Reclaim(ring_buffer)
    }
}

impl<'a, P, T, D, O> TrReclaimIoSliceRef<T> for Reclaim<'a, P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn reclaim_ref(self, slice_ref: &mut IoSliceRef<'_, T, Self>) {
        let x = self.0.state().reader_forward(slice_ref.len());
        assert!(x.is_ok());
    }
}

impl<'a, P, T, D, O> TrReclaimIoSliceMut<T> for Reclaim<'a, P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn reclaim_mut(self, slice_mut: &mut IoSliceMut<'_, T, Self>) {
        let x = self.0.state().writer_forward(slice_mut.len());
        assert!(x.is_ok())
    }
}