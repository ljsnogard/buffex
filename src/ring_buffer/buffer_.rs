use core::{
    borrow::{Borrow, BorrowMut},
    error::Error,
    fmt,
    mem::MaybeUninit,
    ops::Deref,
    ptr::NonNull,
};

use anylr::SomeOf;
use atomex::{StrictOrderings, TrCmpxchOrderings};
use segm_buff::x_deps::abs_buff::x_deps::anylr;

use super::{
    reclaim_::{ReaderForwardFn, ReclSliceMut, ReclSliceRef, WriterForwardFn},
    sync_::{BuffState, IoCtx, IoCtxState},
    rx_::BuffRx,
    tx_::BuffTx,
    Dual, TrRingBuffer,
};

/// Error that may occur while operating rx end of the ring buffer.
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

/// Error that may occur while operating tx end of the ring buffer.
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

type IoPair<B, P, T, O> = (BuffTx<B, P, T, O>, BuffRx<B, P, T, O>);
type TrySplitResult<B, P, T, O> = Result<IoPair<B, P, T, O>, B>;

/// A ring buffer that support both sync and async operation.
pub struct RingBuffer<P, T = u8, O = StrictOrderings>(BuffState<P, T, O>)
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

// Public APIs for RingBuffer
impl<P, T, O> RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    /// ## Safety
    /// 
    /// - `capacity` must be less than or equal buffer's length;
    /// - `capacity` must be less than or usize::MAx >> 3;
    pub const unsafe fn new_unchecked(buffer: P, capacity: usize) -> Self {
        RingBuffer(BuffState::new_unchecked(buffer, capacity))
    }

    /// Create a ring buffer by specifying its internal data storage.
    /// 
    /// Will return `Err` if the buffer is too large ( size greater than or
    /// eqaul to `1 << (usize::BITS - 2)`)
    pub fn try_new(buffer: P) -> Result<Self, usize> {
        Result::Ok(RingBuffer(BuffState::try_new(buffer)?))
    }

    /// Split the buffer into tx end and rx and.
    pub fn split(
        ring_buff: &mut Self,
    ) -> IoPair<&'_ Self, P, T, O> {
        unsafe {
            let mut buffer = NonNull::new_unchecked(ring_buff);
            let i = buffer.as_mut().tx();
            let o = buffer.as_mut().rx();
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
    ) -> TrySplitResult<S, P, T, O>
    where
        S: Borrow<Self> + Deref<Target = Self> + Clone + Send + Sync,
    {
        let x = strong_count(&ring_buff) > 1 || weak_count(&ring_buff) > 0;
        if x {
            Result::Err(ring_buff)
        } else {
            let i = BuffTx::new(IoCtx::new(
                ring_buff.clone(),
                IoCtxState::closing_flag(),
            ));
            let o = BuffRx::new(IoCtx::new(
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

    #[inline]
    pub fn data_size(&self) -> usize {
        self.0.data_size()
    }

    /// Get the `BuffTx` instance associated with this ring buffer.
    /// Dropping it will not cause rx end receiving `RxError::Closing`.
    pub fn tx(&mut self) -> BuffTx<&Self, P, T, O> {
        let ctx_st = IoCtxState::no_close_flag();
        BuffTx::new(IoCtx::new(self, ctx_st))
    }

    /// Get the `BuffRx` instance associated with this ring buffer.
    /// Dropping it will not cause tx end receiving `RxError::Closing`.
    pub fn rx(&mut self) -> BuffRx<&Self, P, T, O> {
        let ctx_st = IoCtxState::no_close_flag();
        BuffRx::new(IoCtx::new(self, ctx_st))
    }
}

// pub(super) APIs for RingBuffer and its Reader/Writer/Peeker

impl<P, T, O> RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
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
    ) -> SomeOf<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        let make_slice = |slice|
            ReclSliceRef::new(slice, Option::None);
        let try_peek_res = self.0.try_peek();
        if let Result::Err(e) = try_peek_res {
            return SomeOf::Right(e);
        }
        let Result::Ok(dual) = try_peek_res else {
            unreachable!()
        };
        let dual = dual
            .into_iter()
            .map(|p| unsafe { p.as_ref() })
            .map(make_slice)
            .collect();
        SomeOf::Left(dual)
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

impl<P, T, O> AsRef<[MaybeUninit<T>]> for RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &[MaybeUninit<T>] {
        self.0.buffer_data()
    }
}

impl<P, T, O> TrRingBuffer<T> for RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type Tx<'a> = BuffTx<&'a Self, P, T, O> where Self: 'a;
    type Rx<'a> = BuffRx<&'a Self, P, T, O> where Self: 'a;

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
    ) -> Option<(Self::Tx<'_>, Self::Rx<'_>)> {
        Option::Some(Self::split(self))
    }
}

unsafe impl<P, T, O> Send for RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    T: Send,
    O: TrCmpxchOrderings,
{}

unsafe impl<P, T, O> Sync for RingBuffer<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
    O: TrCmpxchOrderings,
{}

#[cfg(test)]
mod tests_ {
    use core::{
        borrow::{Borrow, BorrowMut},
        mem::MaybeUninit,
    };

    use atomex::{
        x_deps::funty,
        TrCmpxchOrderings,
    };
    use core_malloc::CoreAlloc;
    use mm_ptr::{Shared, Owned};
    use spmv_oneshot::x_deps::atomex;

    use crate::ring_buffer::*;

    /// 向 buffer 中写入 [0][0,1][0,1,2]...[0,1,..,max_step - 2, max_step - 1]
    async fn write_seq_<B, P, T, O>(
        mut buffer: BuffTx<B, P, T, O>,
        max_len: usize)
    where
        B: Borrow<RingBuffer<P, T, O>>,
        P: BorrowMut<[MaybeUninit<T>]>,
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
                |u, m| {
                    let Result::Ok(x) = T::try_from(u) else { panic!("unable conver from {u}") };
                    m.write(x);
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
                    dst.clone_from(src.split_at(len).0);
                    wrote_len += len;
                }
            }
        }
        log::trace!("writer exits")
    }

    async fn read_seq_<B, P, T, O>(
        mut reader: BuffRx<B, P, T, O>,
        max_len: usize)
    where
        B: Borrow<RingBuffer<P, T, O>>,
        P: BorrowMut<[MaybeUninit<T>]>,
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
                |_, m| { m.write(T::ZERO); },
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
                    Result::Err(RxError::Closing) => {
                        log::trace!("[buffer_::read_seq_] closing");
                        break
                    },
                    Result::Err(e) => panic!(
                        "reader_: step({seq_len}), {:?} - {:?}\n{e:?}",
                        split.0, split.1,
                    ),
                }
            }
            log::trace!("[buffer_::read_seq] #{seq_len}: {target:?} ");
            for (u, x) in target.iter().enumerate() {
                let v = span_offset;
                let Result::<usize, _>::Ok(x) = (*x).try_into() else {
                    panic!()
                };
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

        let Result::Ok(ring_buff) =
            RingBuffer::<Owned<[MaybeUninit<u8>], CoreAlloc>>::try_new(
                Owned::new_uninit_slice(BUFF_SIZE, CoreAlloc::new(),
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

        let Result::Ok(ring_buff) =
            RingBuffer::<Owned<[MaybeUninit<u16>], CoreAlloc>, u16>::try_new(
                Owned::new_uninit_slice(BUFF_SIZE, CoreAlloc::new(),
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

        let Result::Ok(ring_buff) =
            RingBuffer::<Owned<[MaybeUninit<u32>], CoreAlloc>, u32>::try_new(
                Owned::new_uninit_slice(BUFF_SIZE, CoreAlloc::new(),
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
