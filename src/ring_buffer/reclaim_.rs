use core::{
    borrow::BorrowMut,
    mem::MaybeUninit,
};

use abs_buff::TrBuffSegmView;
use atomex::TrCmpxchOrderings;
use segm_buff::{x_deps::abs_buff, SegmMut, SegmRef, TrReclaim};

use super::buffer_::RingBuffer;

pub type ReclSliceRef<'a, P, T, O> =
    SegmRef<&'a [T], T, ReaderForwardFn<'a, P, T, O>>;

pub type ReclSliceMut<'a, P, T, O> =
    SegmMut<&'a mut [MaybeUninit<T>], T, WriterForwardFn<'a, P, T, O>>;

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
        let Option::Some(head) = s.iter_ptr().next() else {
            return;
        };
        debug_assert!({
            let info = self.0.state().load_state_info();
            let buff = self.0.state().buffer_data();
            let rp = &buff[info.rp] as *const MaybeUninit<T> as *const T;
            core::ptr::eq(head, rp)
        });
        let length = s.borrowed_len();
        let x = self.0.state().rx_forward(length);
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
        let Option::Some(head) = s.iter_ptr().next() else {
            #[cfg(test)]
            log::warn!("[WriterForwardFn::reclaim] empty head buff segm");
            return;
        };
        debug_assert!({
            let info = self.0.state().load_state_info();
            let buff = self.0.state().buffer_data();
            let wp = &buff[info.wp] as *const MaybeUninit<T>;
            core::ptr::eq(head, wp)
        });
        let length = s.borrowed_len();
        let x = self.0.state().tx_forward(length);
        assert!(x.is_ok())
    }
}
