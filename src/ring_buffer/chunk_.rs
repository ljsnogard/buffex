use core::borrow::{Borrow, BorrowMut};

use abs_buff::utils::{ReaderAsChunkFiller, ChunkWriter};
use atomex::{
    x_deps::funty,
    TrAtomicData, TrCmpxchOrderings,
};
use core_malloc::CoreAlloc;
use mm_ptr::{Shared, Owned};
use asyncex::x_deps::{atomex, mm_ptr};
use crate::ring_buffer::*;

async fn chunk_writer_<B, P, T, D, O>(writer: Writer<B, P, T, D, O>)
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: funty::Unsigned + TryFrom<usize> + Copy,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    let capacity = writer.buffer().capacity();
    let mut copier = ChunkWriter::new(writer);
    let mut span_length = 1usize;
    let init_each = |u| {
        let Result::Ok(x) = T::try_from(u) else { panic!() };
        x
    };
    loop {
        if span_length > capacity {
            break;
        }
        let source = Owned::new_slice(
            span_length,
            init_each,
            CoreAlloc::new(),
        );
        let w = copier.write_async(&source).await;
        if let Result::Err(report) = w {
            log::error!("[chunk_::chunk_writer_] span_len({span_length}) err: {report:?}");
            break;
        };
        let Result::Ok(copy_len) = w else { unreachable!() };
        log::trace!("[chunk_::chunk_writer_] span_len({span_length}) copy len({copy_len})");
        assert!(copy_len == span_length);
        span_length += 1;
    }
    log::info!("chunk writer exits");
}

async fn reader_filler_<B, P, T, D, O>(reader: Reader<B, P, T, D, O>)
where
    B: Borrow<RingBuffer<P, T, D, O>>,
    P: BorrowMut<[T]>,
    T: funty::Unsigned + TryInto<usize> + Copy,
    D: TrAtomicData + funty::Unsigned,
    O: TrCmpxchOrderings,
{
    let capacity = reader.buffer().capacity();
    let mut filler = ReaderAsChunkFiller::new(reader);
    let mut span_length = 1usize;
    loop {
        if span_length > capacity {
            break;
        }
        let mut target = Owned::new_slice(
            span_length,
            |_| T::ZERO,
            CoreAlloc::new(),
        );
        let r = filler.fill_async(&mut target).await;
        if let Result::Err(report) = r {
            log::error!("[chunk_::reader_filler_] span_len({span_length}) err: {report:?}");
            break;
        };
        let Result::Ok(fill_len) = r else { unreachable!() };
        log::trace!("[chunk_::reader_filler_] span_len({span_length}) filled len({fill_len})");
        assert!(fill_len == span_length);
        for (u, x) in target.iter().enumerate() {
            let Result::Ok(x) = (*x).try_into() else { panic!() };
            assert_eq!(u, x, "#{span_length}: u({u}) != x({x})");
        }
        span_length += 1;
    }
    log::info!("reader filler exits");
}

#[tokio::test]
async fn u8_fill_copy_async_smoke() {
    const BUFF_SIZE: u8 = 255;

    let _ = env_logger::builder().is_test(true).try_init();

    let Result::Ok(ring_buff) = RingBuffer::<Owned<[u8], CoreAlloc>>
        ::try_new(Owned::new_slice(
            usize::from(BUFF_SIZE),
            |_| 0u8,
            CoreAlloc::new(),
        ))
    else {
        panic!("[chunk_::u8_fill_copy_async_smoke] try_new")
    };

    let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
    let Result::Ok((writer, reader)) = RingBuffer
        ::try_split_from_shared(ring_buff)
    else {
        panic!("[chunk_::u8_fill_copy_async_smoke] try_split_shared");
    };
    let reader_handle = tokio::task::spawn(reader_filler_(reader));
    let writer_handle = tokio::task::spawn(chunk_writer_(writer));
    assert!(writer_handle.await.is_ok());
    assert!(reader_handle.await.is_ok());
}

#[tokio::test]
async fn u16_fill_copy_async_smoke() {
    const BUFF_SIZE: u16 = 255;

    let _ = env_logger::builder().is_test(true).try_init();

    let Result::Ok(ring_buff) = RingBuffer::<Owned<[u16], CoreAlloc>, u16>
        ::try_new(Owned::new_slice(
            usize::from(BUFF_SIZE),
            |_| 0u16,
            CoreAlloc::new(),
        ))
    else {
        panic!("[chunk_::u16_fill_copy_async_smoke] try_new")
    };

    let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
    let Result::Ok((writer, reader)) = RingBuffer
        ::try_split_from_shared(ring_buff)
    else {
        panic!("[chunk_::u16_fill_copy_async_smoke] try_split_shared");
    };
    let writer_handle = tokio::task::spawn(chunk_writer_(writer));
    let reader_handle = tokio::task::spawn(reader_filler_(reader));
    assert!(writer_handle.await.is_ok());
    assert!(reader_handle.await.is_ok());
}

#[tokio::test]
async fn u32_fill_copy_async_smoke() {
    const BUFF_SIZE: u32 = 255;

    let _ = env_logger::builder().is_test(true).try_init();

    let Result::Ok(ring_buff) = RingBuffer::<Owned<[u32], CoreAlloc>, u32>
        ::try_new(Owned::new_slice(
            usize::try_from(BUFF_SIZE).unwrap(),
            |_| 0u32,
            CoreAlloc::new(),
        ))
    else {
        panic!("[chunk_::u32_fill_copy_async_smoke] try_new")
    };

    let ring_buff = Shared::new(ring_buff, CoreAlloc::new());
    let Result::Ok((writer, reader)) = RingBuffer
        ::try_split_from_shared(ring_buff)
    else {
        panic!("[chunk_::u32_fill_copy_async_smoke] try_split_shared");
    };
    let writer_handle = tokio::task::spawn(chunk_writer_(writer));
    let reader_handle = tokio::task::spawn(reader_filler_(reader));
    assert!(writer_handle.await.is_ok());
    assert!(reader_handle.await.is_ok());
}
