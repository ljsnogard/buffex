use abs_buff::{
    buffer::{TrAsBuffer, TrAsBufferMut},
    x_deps::abs_cancel,
};
use abs_cancel::TrMayCancel;

use super::ring_core_::IoPos;

pub trait TrConsumerHook<T> {
    type Buff: TrAsBuffer<T>;

    fn init_once(&mut self, buf: &Self::Buff, pos: &IoPos);

    fn handle_event(&self, buf: &Self::Buff, pos: &IoPos);
}

pub trait TrProducerHook<T> {
    type Buff: TrAsBufferMut<T>;

    fn init_once(&mut self, buf: &Self::Buff, pos: &IoPos);

    fn handle_event(&self, buf: &Self::Buff, pos: &IoPos);
}

pub trait TrPark {
    type ParkAsync<'f>: TrMayCancel<'f, MayCancelOutput = Option<Self::Err>>
    where
        Self: 'f;

    type Err;

    fn park_async(&mut self) -> Self::ParkAsync<'_>;
}
