use core::{borrow::BorrowMut, mem::MaybeUninit};

use abs_buff::{
    Demand,
    x_deps::abs_cancel,
};
use abs_cancel::TrMayCancel;

use super::ring_core_::RingState;

pub trait TrConsumerHook<T> {
    type Buff: BorrowMut<[MaybeUninit<T>]>;

    fn init_once(&mut self, buff: &Self::Buff, state: &RingState);

    fn handle_event(&self, buff: &Self::Buff, state: &RingState);
}

pub trait TrProducerHook<T> {
    type Buff: BorrowMut<[MaybeUninit<T>]>;

    fn init_once(&mut self, buf: &Self::Buff, state: &RingState);

    fn handle_event(&self, buff: &Self::Buff, state: &RingState);
}

pub trait TrPark {
    type ParkAsync<'f>: TrMayCancel<'f, MayCancelOutput = Option<Self::Err>>
    where
        Self: 'f;

    type Err;

    fn park_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::ParkAsync<'f>;
}
