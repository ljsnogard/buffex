use core::{
    borrow::{Borrow, BorrowMut},
    error::Error,
    fmt,
    marker::{PhantomData, PhantomPinned},
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::AtomicUsize,
};

use atomex::{AtomicCount, StrictOrderings, TrCmpxchOrderings};
use asyncex_channel::x_deps::atomex;

use super::{
    read_::BuffRead,
    reclaim_::{ReaderForwardFn, ReclSliceMut, ReclSliceRef, WriterForwardFn},
    sync_::{BuffState, Demand},
    write_::BuffWrite,
    Dual, TrRingBuffer,
};

/// Error that may occur while operating with the output end of the ring buffer.
#[derive(Debug)]
pub enum RxError<T> {
    /// Illegal argument.
    Argument,

    /// The input end has closed and the ring buffer is already empty.
    Closing,

    /// The ring buffer is empty and thus temporarily unable to output
    Drained(T),
}

impl<T> fmt::Display for RxError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RxError::Argument => write!(f, "RxError::Argument"),
            RxError::Closing => write!(f, "RxError::Closing"),
            RxError::Drained(t) => write!(f, "RxError::Drained({t:?})"),
        }
    }
}

impl<T> Error for RxError<T>
where
    T: fmt::Debug,
{}

/// Error that may occur while operating with the input end of the ring buffer.
#[derive(Debug)]
pub enum TxError<T> {
    /// Illegal argument.
    Argument,

    /// The output end has closed and buffer is already full.
    Closing,

    /// The ring buffer is full and thus temporarily unable to input.
    Stuffed(T),
}

impl<T> fmt::Display for TxError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxError::Argument => write!(f, "TxError::Argument"),
            TxError::Closing => write!(f, "TxError::Closing"),
            TxError::Stuffed(t) => write!(f, "TxError::Stuffed({t:?})"),
        }
    }
}

impl<T> Error for TxError<T>
where
    T: fmt::Debug,
{}

type IoPair<X, B, P, T, O> = (
    BuffWrite<X, B, P, T, O>,
    BuffRead<X, B, P, T, O>,
);
type TrySplitResult<X, B, P, T, O> = Result<IoPair<X, B, P, T, O>, B>;

pub struct RingBuffer<P, T = u8, O = StrictOrderings>(BuffState<P, T, O>)
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

// Public APIs for RingBuffer
impl<P, T, O> RingBuffer<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub fn try_new(buffer: P) -> Result<Self, usize> {
        Result::Ok(RingBuffer(BuffState::try_new(buffer)?))
    }

    pub fn split(
        ring_buff: &mut Self,
    ) -> IoPair<IoCtx<&'_ Self, P, T, O>, &'_ Self, P, T, O> {
        unsafe {
            let mut buffer = NonNull::new_unchecked(ring_buff);
            let i = buffer.as_mut().input();
            let o = buffer.as_mut().output();
            (i, o)
        }
    }

    /// Split a `RingBuffer` shared by the smart pointer `S`, where `S` can be
    /// `Arc<T>` or `Shared<T>`, and when strong count is 1 and weak count is 0.
    /// 
    /// ## Safety
    /// 
    /// * Don't cheat on `strong_count` and `weak_count`.
    pub fn try_split<S>(
        ring_buff: S,
        strong_count: impl FnOnce(&S) -> usize,
        weak_count: impl FnOnce(&S) -> usize,
    ) -> TrySplitResult<IoCtx<S, P, T, O>, S, P, T, O>
    where
        S: Borrow<Self> + Deref<Target = Self> + Clone + Send + Sync,
    {
        let x = strong_count(&ring_buff) > 1 || weak_count(&ring_buff) > 0;
        if x {
            Result::Err(ring_buff)
        } else {
            let i = BuffWrite::new(IoCtx::new(
                ring_buff.clone(),
                IoCtxState::closing_flag(),
            ));
            let o = BuffRead::new(IoCtx::new(
                ring_buff,
                IoCtxState::closing_flag(),
            ));
            Result::Ok((i, o))
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    #[inline(always)]
    pub fn data_size(&self) -> usize {
        self.0.data_size()
    }

    /// Get the `BuffWrite` instance associated with this ring buffer.
    /// Dropping it will not cause rx end receiving `RxError::Closing`.
    pub fn input(
        &mut self,
    ) -> BuffWrite<IoCtx<&Self, P, T, O>, &Self, P, T, O> {
        let ctx_st = IoCtxState::no_close_flag();
        BuffWrite::new(IoCtx::new(self, ctx_st))
    }

    /// Get the `BuffRead` instance associated with this ring buffer.
    /// Dropping it will not cause tx end receiving `RxError::Closing`.
    pub fn output(
        &mut self,
    ) -> BuffRead<IoCtx<&Self, P, T, O>, &Self, P, T, O> {
        let ctx_st = IoCtxState::no_close_flag();
        BuffRead::new(IoCtx::new(self, ctx_st))
    }
}

// pub(super) APIs for RingBuffer and its Reader/Writer/Peeker

impl<P, T, O> RingBuffer<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) fn try_read_(
        &self,
        length: usize,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        let make_slice = |slice| ReclSliceRef::new(
            slice,
            Option::Some(ReaderForwardFn::new(self))
        );
        let dual = self
            .0
            .try_read(length)?
            .into_iter()
            .map(|p| unsafe { p.as_ref() })
            .map(make_slice)
            .collect();
        Result::Ok(dual)
    }

    pub(super) fn try_peek_(
        &self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        let make_slice = |slice| ReclSliceRef::new(slice, Option::None);
        let dual = self
            .0
            .try_peek()?
            .into_iter()
            .map(|p| unsafe { p.as_ref() })
            .map(make_slice)
            .collect();
        Result::Ok(dual)
    }

    pub(super) fn try_write_(
        &self,
        length: usize,
    ) -> Result<Dual<ReclSliceMut<'_, P, T, O>>, TxError<usize>> {
        let make_slice = |slice_mut| ReclSliceMut::new(
            slice_mut,
            Option::Some(WriterForwardFn::new(self)),
        );
        let dual = self
            .0
            .try_write(length)?
            .into_iter()
            .map(|mut p| unsafe { p.as_mut() })
            .map(make_slice)
            .collect();
        Result::Ok(dual)
    }

    pub(super) fn state(&self) -> &BuffState<P, T, O> {
        &self.0
    }
}

impl<P, T, O> AsRef<[T]> for RingBuffer<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &[T] {
        self.0.buffer_data()
    }
}

impl<P, T, O> TrRingBuffer<T> for RingBuffer<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Input<'a> = BuffWrite<IoCtx<&'a Self, P, T, O>, &'a Self, P, T, O> where Self: 'a;
    type Output<'a> = BuffRead<IoCtx<&'a Self, P, T, O>, &'a Self, P, T, O> where Self: 'a;

    #[inline]
    fn capacity(&self) -> usize {
        RingBuffer::capacity(self)
    }

    #[inline]
    fn data_size(&self) -> usize {
        RingBuffer::data_size(self)
    }

    #[inline]
    fn try_split_io(
        &mut self,
    ) -> Option<(Self::Input<'_>, Self::Output<'_>)> {
        Option::Some(Self::split(self))
    }
}

pub struct IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    _pinned: PhantomPinned,
    _use_p_: PhantomData<P>,
    _use_t_: PhantomData<[T]>,
    buffer_: B,
    ctx_st_: IoCtxState,
    demand_: Option<Demand<O>>,
}

impl<B, P, T, O> IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(buffer: B, ctx_st: IoCtxState) -> Self {
        IoCtx {
            _pinned: PhantomPinned,
            _use_p_: PhantomData,
            _use_t_: PhantomData,
            buffer_: buffer,
            ctx_st_: ctx_st,
            demand_: Option::None,
        }
    }

    pub(super) fn buffer(&self) -> &RingBuffer<P, T, O> {
        self.buffer_.borrow()
    }

    pub(super) fn state(&self) -> &IoCtxState {
        &self.ctx_st_
    }

    #[inline]
    pub(super) fn demand(&self) -> Option<&Demand<O>> {
        self.demand_.as_ref()
    }

    pub(super) fn try_init_demand(
        self: Pin<&mut Self>,
        demand: Demand<O>,
    ) -> Result<&Demand<O>, Demand<O>> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.demand_.is_none() {
            this.demand_ = Option::Some(demand);
            let Option::Some(demand_ref) = &this.demand_ else {
                unreachable!()
            };
            Result::Ok(demand_ref)
        } else {
            Result::Err(demand)
        }
    }
}

impl<B, P, T, O> AsMut<B> for IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_mut(&mut self) -> &mut B {
        &mut self.buffer_
    }
}

pub(super) struct IoCtxState(AtomicUsize);

impl IoCtxState {
    /// Set the MSB to 1 to flag NO_CLOSE
    const NO_CLOSE_FLAG: usize = 1usize << (usize::BITS - 1);

    const fn closing_flag() -> Self {
        Self(AtomicUsize::new(0usize))
    }

    const fn no_close_flag() -> Self {
        Self(AtomicUsize::new(Self::NO_CLOSE_FLAG))
    }

    #[inline(always)]
    fn atomic_count_(&self) -> AtomicCount<usize, &mut AtomicUsize> {
        let x = self as *const _ as *mut Self;
        unsafe { AtomicCount::new(&mut (*x).0) }
    }

    #[inline(always)]
    fn get_use_count_(s: usize) -> usize {
        s & (!Self::NO_CLOSE_FLAG)
    }

    /// Returns if the flag indicate the input or output end should close;
    /// true, should close, false, no close.
    #[inline(always)]
    pub fn test_closing_flagged(s: usize) -> bool {
        s | (!Self::NO_CLOSE_FLAG) != usize::MAX
    }

    pub fn incr_use_count(&self) -> IoCtrl {
        let c = self.atomic_count_().inc();
        IoCtrl::NoOp(Self::get_use_count_(c))
    }

    pub fn decr_use_count(&self) -> IoCtrl {
        let s = self.atomic_count_().dec();
        let c = Self::get_use_count_(s);
        if c == 1 && Self::test_closing_flagged(s) {
            IoCtrl::MarkClose(c)
        } else {
            IoCtrl::NoOp(c)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum IoCtrl {
    MarkClose(usize),
    NoOp(usize),
}

impl IoCtrl {
    #[allow(dead_code)]
    pub const fn use_count(&self) -> usize {
        match self {
            Self::MarkClose(c) => *c,
            Self::NoOp(c) => *c,
        }
    }
}

impl fmt::Display for IoCtrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoCtrl::MarkClose(c) => {
                let c = *c;
                write!(f, "IoCtrl::MarkClose({c})")
            }
            IoCtrl::NoOp(c) => {
                let c = *c;
                write!(f, "IoCtrl::NoOp({c})")
            }
        }
    }
}

#[cfg(test)]
mod tests_ {
    use core::borrow::{Borrow, BorrowMut};

    use asyncex_channel::x_deps::{mm_ptr, atomex};
    use atomex::{
        x_deps::funty,
        TrCmpxchOrderings,
    };
    use core_malloc::CoreAlloc;
    use mm_ptr::{Shared, Owned};

    use crate::ring_buffer::{*, buffer_::IoCtx};

    /// 向 buffer 中写入 [0][0,1][0,1,2]...[0,1,..,max_step - 2, max_step - 1]
    async fn write_seq_<X, B, P, T, O>(
        mut buffer: BuffWrite<X, B, P, T, O>,
        max_len: usize)
    where
        X: BorrowMut<IoCtx<B, P, T, O>>,
        B: Borrow<RingBuffer<P, T, O>>,
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryFrom<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let mut seq_len = 1usize;
        log::trace!("[buffer_::tests_::write_seq_] starts");
        loop {
            if seq_len > max_len {
                break;
            }
            let source = Owned::new_slice(
                seq_len,
                |u| {
                    let Result::Ok(x) = T::try_from(u) else { panic!("unable conver from {u}") };
                    x
                },
                CoreAlloc::new(),
            );
            // The number of items that has been written into.
            let mut wrote_len = 0usize;
            // 每一次循环都会把完整的 source 写进 buffer
            loop {
                let req_size = source.len() - wrote_len;
                if req_size == 0 {
                    seq_len += 1;
                    break;
                }
                let try_write = buffer.write_async(req_size).await;
                let Result::Ok(dst_iter) = try_write else {
                    let e = try_write.err().unwrap();
                    panic!("writer_: step({seq_len}), wrote_len({wrote_len}), req_size({req_size}), e({e:?})")
                };
                for mut dst in dst_iter.into_iter() {
                    let dst_len = dst.len();
                    log::trace!("[buffer_::write_seq_] seq_len({seq_len}), wrote_len({wrote_len}), req_size({req_size}), dst_len({dst_len})");
                    let split = source.split_at(wrote_len);
                    let src = split.1;
                    let len = dst.len();
                    assert!(len <= src.len());
                    dst.clone_from_slice(src.split_at(len).0);
                    wrote_len += len;
                }
            }
        }
        log::trace!("writer exits")
    }

    async fn read_seq_<X, B, P, T, O>(
        mut reader: BuffRead<X, B, P, T, O>,
        max_len: usize)
    where
        X: BorrowMut<IoCtx<B, P, T, O>>,
        B: Borrow<RingBuffer<P, T, O>>,
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryInto<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let mut seq_len = max_len;
        let mut c = 0usize;
        let mut span_length = 1usize;
        let mut span_offset = 0usize;
        log::trace!("[buffer_::read_seq_] starts");
        loop {
            if seq_len == 0usize {
                break;
            }
            let mut target = Owned::new_slice(
                seq_len,
                |_| T::ZERO,
                CoreAlloc::new(),
            );
            let mut read_len = 0usize;
            loop {
                let split = target.split_at_mut(read_len);
                let dst: &mut [T] = split.1;
                log::trace!("[buffer_::read_seq_] before read_async: seq_len({seq_len}), read_len({read_len})");
                match reader.read_async(dst.len()).await {
                    Result::Ok(dual) => {
                        let mut dst_w = 0usize;
                        for src in dual.into_iter() {
                            let src_len = src.len();
                            log::trace!("[buffer_::read_seq_] seq_len({seq_len}), dst_w({dst_w}), read_len({read_len}), src_len({src_len})");
                            assert!(dst_w + src_len <= dst.len());
                            dst[dst_w..dst_w + src_len].clone_from_slice(&src);
                            dst_w += src_len;
                        }
                        read_len += dst_w;
                        c += dst_w;
                        if read_len == target.len() { break; }
                    },
                    Result::Err(RxError::Closing) => break,
                    Result::Err(e) => panic!(
                        "reader_: step({seq_len}), {:?} - {:?}\n{e:?}",
                        split.0, split.1,
                    ),
                }
            }
            log::trace!("[buffer_::read_seq] #{seq_len}: {target:?} ");
            for (u, x) in target.iter().enumerate() {
                let v = span_offset;
                let Result::Ok(x) = (*x).try_into() else { panic!() };
                assert_eq!(v, x, "#{u}: v({v}) != x({x})");
                span_offset += 1;
                if span_offset == span_length {
                    log::trace!("[buffer_::read_seq_] reader done validating span_length({span_length})");
                    span_length += 1;
                    span_offset = 0;
                }
            }
            if c >= seq_len {
                seq_len -= 1;
                c = 0usize;
            }
        }
        log::trace!("read_seq_ exits")
    }

    #[tokio::test]
    async fn u8_read_write_async_smoke() {
        const BUFF_SIZE: usize = 32;
        const MAX_LEN: usize = 16usize;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u8], CoreAlloc>>
            ::try_new(Owned::new_slice(
                BUFF_SIZE,
                |_| 0u8,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u8_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split(ring_buff, Shared::strong_count, Shared::weak_count)
        else {
            panic!("[tests_::u8_read_write_async_smoke] try_split_shared");
        };
        let reader_handle = tokio::task::spawn(read_seq_(reader, MAX_LEN));
        let writer_handle = tokio::task::spawn(write_seq_(writer, MAX_LEN));
        assert!(writer_handle.await.is_ok());
        assert!(reader_handle.await.is_ok());
    }

    #[tokio::test]
    async fn u16_read_write_async_smoke() {
        const BUFF_SIZE: usize = 32;
        const MAX_LEN: usize = 16usize;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u16], CoreAlloc>, u16>
            ::try_new(Owned::new_slice(
                BUFF_SIZE,
                |_| 0u16,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u16_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split(ring_buff, Shared::strong_count, Shared::weak_count)
        else {
            panic!("[tests_::u16_read_write_async_smoke] try_split_shared");
        };
        let whndl = tokio::task::spawn(write_seq_(writer, MAX_LEN));
        let rhndl = tokio::task::spawn(read_seq_(reader, MAX_LEN));
        assert!(whndl.await.is_ok());
        assert!(rhndl.await.is_ok());
    }

    #[tokio::test]
    async fn u32_read_write_async_smoke() {
        const BUFF_SIZE: usize = 1024;
        const MAX_LEN: usize = 16usize;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u32], CoreAlloc>, u32>
            ::try_new(Owned::new_slice(
                BUFF_SIZE,
                |_| 0u32,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u32_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split(ring_buff, Shared::strong_count, Shared::weak_count)
        else {
            panic!("[tests_::u32_read_write_async_smoke] try_split_shared");
        };
        let writer_handle = tokio::task::spawn(write_seq_(writer, MAX_LEN));
        let reader_handle = tokio::task::spawn(read_seq_(reader, MAX_LEN));
        assert!(writer_handle.await.is_ok());
        assert!(reader_handle.await.is_ok());
    }
}
