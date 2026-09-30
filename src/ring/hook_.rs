use abs_buff::{Demand, x_deps::abs_cancel};
use abs_cancel::TrMayCancel;

use crate::ring::ring_core_::RingState;

pub trait TrPark {
    type ParkAsync<'f>: TrMayCancel<'f, MayCancelOutput = Option<Self::Err>>
    where
        Self: 'f;

    type Err;

    fn wake(&self, state: &RingState);

    fn park_async<'f>(
        &'f mut self,
        demand: &'f Demand<usize>,
    ) -> Self::ParkAsync<'f>;
}
