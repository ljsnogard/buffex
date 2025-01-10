use core::{
    borrow::BorrowMut,
    mem::MaybeUninit,
};

use abs_buff::TrBuffSegmView;
use atomex::TrCmpxchOrderings;
use recl_slices::{SliceMut, SliceRef, TrReclaim};

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

impl<P, T, O> TrReclaim<T> for ReaderForwardFn<'_, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn reclaim<S: TrBuffSegmView<Item = T>>(&mut self, s: &mut S) {
        debug_assert!({
            let slice = s.borrow();
            let info = self.0.state().load_state_info();
            let buff = self.0.state().buffer_data();
            let rp = &buff[info.rp] as *const MaybeUninit<T> as *const T;
            let head = &slice[0] as *const T;
            core::ptr::eq(rp, head)
        });
        let x = self.0.state().rx_forward(s.len());
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

impl<P, T, O> TrReclaim<MaybeUninit<T>> for WriterForwardFn<'_, P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn reclaim<S: TrBuffSegmView<Item = MaybeUninit<T>>>(&mut self, s: &mut S) {
        debug_assert!({
            let slice = s.borrow();
            let info = self.0.state().load_state_info();
            let buff = self.0.state().buffer_data();
            let wp = &buff[info.wp] as *const MaybeUninit<T>;
            let head = &slice[0] as *const MaybeUninit<T>;
            core::ptr::eq(wp, head)
        });
        let x = self.0.state().tx_forward(s.len());
        assert!(x.is_ok())
    }
}
