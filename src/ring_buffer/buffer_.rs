use core::{
    borrow::{Borrow, BorrowMut},
    ops::Deref,
    ptr::NonNull,
};

use abs_buff::{IoSliceMut, IoSliceRef, NoReclaim};
use atomex::{
    x_deps::funty,
    StrictOrderings, TrAtomicData, TrCmpxchOrderings,
};
use abs_mm::mem_alloc::TrMalloc;
use mm_ptr::{
    x_deps::{abs_mm, atomex},
    Shared,
};
use asyncex::x_deps::mm_ptr;

use super::{
    reader_::Reader,
    sync_::*,
    writer_::Writer,
    TrAsyncRingBuffer,
};

pub use super::reclaim_::Reclaim;

#[derive(Debug)]
pub enum RxError<T> {
    Argument,
    Closing,
    Drained(T),
}

#[derive(Debug)]
pub enum TxError<T> {
    Argument,
    Closing,
    Stuffed(T),
}

type WriterReader<B, P, T, D, O> = (
    Writer<B, P, T, D, O>,
    Reader<B, P, T, D, O>,
);
type TrySplitResult<B, P, T, D, O> = Result<WriterReader<B, P, T, D, O>, B>;

pub struct RingBuffer<
    P,
    T = u8,
    D = usize,
    O = StrictOrderings,
>(BuffState<P, T, D, O>)
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings;

// Public APIs for RingBuffer
impl<P, T, D, O> RingBuffer<P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    pub fn try_new(buffer: P) -> Result<Self, usize> {
        Result::Ok(RingBuffer(BuffState::try_new(buffer)?))
    }

    pub fn split(
        ring_buff: &mut Self,
    ) -> WriterReader<&'_ mut Self, P, T, D, O> {
        unsafe { 
            let mut p = NonNull::new_unchecked(ring_buff);
            let writer = Writer::new(p.as_mut());
            let reader = Reader::new(p.as_mut());
            (writer, reader)
        }
    }

    pub fn try_split_from_shared<A: TrMalloc + Clone>(
        ring_buff: Shared<Self, A>,
    ) -> TrySplitResult<Shared<Self, A>, P, T, D, O> {
        Self::try_split_from(
            ring_buff,
            Shared::strong_count,
            Shared::weak_count,
        )
    }

    /// Split a `RingBuffer` shared by the smart pointer `S`, where `S` can be
    /// `Arc<T>` or `Shared<T>`, and when strong count is 1 and weak count is 0.
    /// 
    /// ## Safety
    /// * `strong_count` and `weak_count` can make a cheat.
    pub fn try_split_from<S>(
        ring_buff: S,
        strong_count: impl FnOnce(&S) -> usize,
        weak_count: impl FnOnce(&S) -> usize,
    ) -> TrySplitResult<S, P, T, D, O>
    where
        S: Borrow<Self> + Deref<Target = Self> + Clone + Send + Sync,
    {
        let x = strong_count(&ring_buff) > 1 || weak_count(&ring_buff) > 0;
        if x {
            Result::Err(ring_buff)
        } else {
            let writer = Writer::new(ring_buff.clone());
            let reader = Reader::new(ring_buff);
            Result::Ok((writer, reader))
        }
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.0.capacity_usize()
    }

    #[inline(always)]
    pub fn data_size(&self) -> usize {
        self.0.data_size_usize()
    }

    pub fn writer(&mut self) -> Writer<&'_ mut Self, P, T, D, O> {
        Writer::new(self)
    }

    pub fn reader(&mut self) -> Reader<&'_ mut Self, P, T, D, O> {
        Reader::new(self)
    }
}

// pub(super) APIs for RingBuffer and its Reader/Writer/Peeker

impl<P, T, D, O> RingBuffer<P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn can_read_(&self) -> bool {
        self.0.can_read()
    }

    #[inline(always)]
    pub(super) fn can_write_(&self) -> bool {
        self.0.can_write()
    }

    #[inline(always)]
    pub(super) fn can_peek_(&self, skip: usize) -> bool {
        self.0.can_peek(skip)
    }

    pub(super) fn try_read_(
        &self,
        length: usize,
    ) -> Result<SliceRef<'_, P, T, D, O>, RxError<D>> {
        let slice_ptr = self.0.try_read(length)?;
        let slice_ref = unsafe { slice_ptr.as_ref() };
        let reclaimer = Reclaim::new(self);
        Result::Ok(IoSliceRef::new_with_reclaimer(slice_ref, reclaimer))
    }

    pub(super) fn try_peek_(
        &self,
        skip: usize,
    ) -> Result<IoSliceRef<'_, T, NoReclaim<T>>, RxError<D>> {
        let slice = self.0.try_peek(skip)?;
        let slice = unsafe { slice.as_ref() };
        Result::Ok(IoSliceRef::new(slice))
    }

    pub(super) fn try_write_(
        &self,
        length: usize,
    ) -> Result<SliceMut<'_, P, T, D, O>, TxError<D>> {
        let mut slice_ptr = self.0.try_write(length)?;
        let slice_mut = unsafe { slice_ptr.as_mut() };
        let reclaimer = Reclaim::new(self);
        Result::Ok(IoSliceMut::new_with_reclaimer(slice_mut, reclaimer))
    }

    pub(super) fn state(&self) -> &BuffState<P, T, D, O> {
        &self.0
    }
}

impl<P, T, D, O> AsRef<[T]> for RingBuffer<P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &[T] {
        self.0.buffer_data()
    }
}

impl<P, T, D, O> TrAsyncRingBuffer<T> for RingBuffer<P, T, D, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    type Writer<'a> = Writer<&'a mut Self, P, T, D, O> where Self: 'a;
    type Reader<'a> = Reader<&'a mut Self, P, T, D, O> where Self: 'a;

    #[inline(always)]
    fn capacity(&self) -> usize {
        RingBuffer::capacity(self)
    }

    #[inline(always)]
    fn data_size(&self) -> usize {
        RingBuffer::data_size(self)
    }

    #[inline(always)]
    fn writer(&mut self) -> Self::Writer<'_> {
        RingBuffer::writer(self)
    }

    #[inline(always)]
    fn reader(&mut self) -> Self::Reader<'_> {
        RingBuffer::reader(self)
    }
}

pub type SliceRef<'a, P, T, D, O> = IoSliceRef<'a, T, Reclaim<'a, P, T, D, O>>;
pub type SliceMut<'a, P, T, D, O> = IoSliceMut<'a, T, Reclaim<'a, P, T, D, O>>;

#[cfg(test)]
mod tests_ {
    use core::borrow::{Borrow, BorrowMut};

    use atomex::{
        x_deps::funty,
        TrAtomicData, TrCmpxchOrderings,
    };
    use core_malloc::CoreAlloc;
    use mm_ptr::{Shared, Owned};
    use asyncex::x_deps::{mm_ptr, atomex};
    use crate::ring_buffer::*;

    async fn writer_<B, P, T, D, O>(mut writer: Writer<B, P, T, D, O>)
    where
        B: Borrow<RingBuffer<P, T, D, O>>,
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryFrom<usize> + Copy,
        D: TrAtomicData + funty::Unsigned,
        O: TrCmpxchOrderings,
    {
        let capacity = writer.buffer().capacity();
        let mut step = 1usize;
        let mut c = 0usize;
        loop {
            if step > capacity {
                break;
            }
            let source = Owned::new_slice(
                step,
                |u| {
                    let Result::Ok(x) = T::try_from(u) else { panic!() };
                    x
                },
                CoreAlloc::new(),
            );
            let mut wrote_len = 0usize;
            loop {
                let split = source.split_at(wrote_len);
                let src = split.1;
                match writer.write_async(src.len()).await {
                    Result::Ok(mut dst) => {
                        let len = dst.len();
                        assert!(len <= src.len());
                        dst.clone_from_slice(src.split_at(len).0);
                        wrote_len += len;
                        c += len;
                        if wrote_len == source.len() {
                            // std::println!("writer #{step}: {:?} ({})", source.as_ref(), *s);
                            break;
                        }
                    },
                    Result::Err(e) => panic!(
                        "writer_: step({step}), {:?} - {:?}\n{e:?}",
                        split.0, split.1
                    ),
                }
            }
            if c >= step {
                step += 1;
                c = 0usize;
            }
        }
        log::trace!("writer exits")
    }

    async fn reader_<B, P, T, D, O>(mut reader: Reader<B, P, T, D, O>)
    where
        B: Borrow<RingBuffer<P, T, D, O>>,
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryInto<usize> + Copy,
        D: TrAtomicData + funty::Unsigned,
        O: TrCmpxchOrderings,
    {
        let capacity = reader.buffer().capacity();
        let mut step = capacity;
        let mut c = 0usize;
        let mut span_length = 1usize;
        let mut span_offset = 0usize;
        loop {
            if step == 0usize {
                break;
            }
            let mut target = Owned::new_slice(
                step,
                |_| T::ZERO,
                CoreAlloc::new(),
            );
            let mut read_len = 0usize;
            loop {
                let split = target.split_at_mut(read_len);
                let dst: &mut [T] = split.1;
                match reader.read_async(dst.len()).await {
                    Result::Ok(src) => {
                        let len = src.len();
                        assert!(len <= dst.len());
                        dst[..len].clone_from_slice(&src);
                        read_len += len;
                        c += len;
                        if read_len == target.len() { break; }
                    },
                    Result::Err(RxError::Closing) => break,
                    Result::Err(e) => panic!(
                        "reader_: step({step}), {:?} - {:?}\n{e:?}",
                        split.0, split.1,
                    ),
                }
            }
            // std::println!("reader #{step}: {:?} ({})", target, *s);
            for (u, x) in target.iter().enumerate() {
                let v = span_offset;
                let Result::Ok(x) = (*x).try_into() else { panic!() };
                assert_eq!(v, x, "#{u}: v({v}) != x({x})");
                span_offset += 1;
                if span_offset == span_length {
                    log::trace!("reader done validating span_length({span_length})");
                    span_length += 1;
                    span_offset = 0;
                }
            }
            if c >= step {
                step -= 1;
                c = 0usize;
            }
        }
        log::trace!("reader exits")
    }

    #[tokio::test]
    async fn u8_read_write_async_smoke() {
        const BUFF_SIZE: u8 = 255;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u8], CoreAlloc>>
            ::try_new(Owned::new_slice(
                usize::from(BUFF_SIZE),
                |_| 0u8,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u8_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split_from_shared(ring_buff)
        else {
            panic!("[tests_::u8_read_write_async_smoke] try_split_shared");
        };
        let writer_handle = tokio::task::spawn(writer_(writer));
        let reader_handle = tokio::task::spawn(reader_(reader));
        assert!(writer_handle.await.is_ok());
        assert!(reader_handle.await.is_ok());
    }

    #[tokio::test]
    async fn u16_read_write_async_smoke() {
        const BUFF_SIZE: u16 = 1024;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u16], CoreAlloc>, u16>
            ::try_new(Owned::new_slice(
                usize::from(BUFF_SIZE),
                |_| 0u16,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u16_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split_from_shared(ring_buff)
        else {
            panic!("[tests_::u16_read_write_async_smoke] try_split_shared");
        };
        let writer_handle = tokio::task::spawn(writer_(writer));
        let reader_handle = tokio::task::spawn(reader_(reader));
        assert!(writer_handle.await.is_ok());
        assert!(reader_handle.await.is_ok());
    }

    #[tokio::test]
    async fn u32_read_write_async_smoke() {
        const BUFF_SIZE: u32 = 1024;

        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(ring_buff) = RingBuffer::<Owned<[u32], CoreAlloc>, u32>
            ::try_new(Owned::new_slice(
                usize::try_from(BUFF_SIZE).unwrap(),
                |_| 0u32,
                CoreAlloc::new(),
            ))
        else {
            panic!("[tests_::u32_read_write_async_smoke] try_new")
        };

        let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
        let Result::Ok((writer, reader)) = RingBuffer
            ::try_split_from_shared(ring_buff)
        else {
            panic!("[tests_::u32_read_write_async_smoke] try_split_shared");
        };
        let writer_handle = tokio::task::spawn(writer_(writer));
        let reader_handle = tokio::task::spawn(reader_(reader));
        assert!(writer_handle.await.is_ok());
        assert!(reader_handle.await.is_ok());
    }
}
