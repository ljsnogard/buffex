use abs_buff::{TrBuffPeeker, TrBuffReader, TrBuffWriter};

pub trait TrAsyncRingBuffer<T: Clone = u8> {
    type Writer<'a>: 'a + TrAsyncRingBuffWriter<T, RingBuffer = Self>
    where
        Self: 'a;

    type Reader<'a>: 'a + TrAsyncRingBuffReader<T, RingBuffer = Self>
    where
        Self: 'a;

    fn capacity(&self) -> usize;

    fn data_size(&self) -> usize;

    fn writer(&mut self) -> Self::Writer<'_>;

    fn reader(&mut self) -> Self::Reader<'_>;
}

pub trait TrAsyncRingBuffPeeker<T: Clone = u8>
where
    Self: TrBuffPeeker<T>
        + core::borrow::Borrow<Self::RingBuffer>
        + core::convert::AsMut<Self::Reader>,
{
    type RingBuffer: TrAsyncRingBuffer<T>;
    type Reader: TrAsyncRingBuffReader<T>;

    fn try_peek(&mut self, skip: usize) -> Result<
        <Self as TrBuffPeeker<T>>::BuffRef<'_>,
        <Self as TrBuffPeeker<T>>::Error,
    >;
}

pub trait TrAsyncRingBuffReader<T: Clone = u8>
where
    Self: TrBuffReader<T>
        + core::borrow::Borrow<Self::RingBuffer>
        + core::convert::AsMut<Self::Peeker>,
{
    type RingBuffer: TrAsyncRingBuffer<T>;
    type Peeker: TrAsyncRingBuffPeeker<T>;

    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffReader<T>>::BuffRef<'_>,
        <Self as TrBuffReader<T>>::Error,
    >;
}

pub trait TrAsyncRingBuffWriter<T: Clone = u8>
where
    Self: TrBuffWriter<T>
        + core::borrow::Borrow<Self::RingBuffer>,
{
    type RingBuffer: TrAsyncRingBuffer<T>;

    fn try_write(&mut self, length: usize) -> Result<
        <Self as TrBuffWriter<T>>::BuffMut<'_>,
        <Self as TrBuffWriter<T>>::Error,
    >;
}
