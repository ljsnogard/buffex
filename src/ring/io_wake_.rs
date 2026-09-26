use abs_buff::{
    buffer::{TrAsBuffer, TrAsBufferMut},
    x_deps::abs_cancel,
};
use abs_cancel::TrMayCancel;

use super::ring_core_::IoPos;

pub trait TrConsumerNotify<T> {
    type Buff: TrAsBuffer<T>;

    fn notify(&self, pos: IoPos);
}

pub trait TrProducerNotify<T> {
    type Buff: TrAsBufferMut<T>;

    fn notify(&self, pos: IoPos);
}

pub trait TrPark {
    type ParkAsync<'f>: TrMayCancel<'f, MayCancelOutput = bool> where Self: 'f;

    fn park_async(&mut self) -> Self::ParkAsync<'_>;
}
