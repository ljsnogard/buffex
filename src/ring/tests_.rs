use std::{
    boxed::Box,
    mem::MaybeUninit,
};

use abs_buff::{
    Demand,
    buffer::{TrBuffSegmRef, TrBuffSegmMut},
};

use crate::{
    ring::{passive::*, *}, test_support_::dual_runtime_test_,
};

async fn smoke_test_() {
    const BUFF_SIZE: usize = 8;
    let buff = Box::<[u8]>::new_uninit_slice(BUFF_SIZE);
    let mut ring = Ring::new_unchecked(buff, Producer::new(), Consumer::new());
    {
        let w_demand = Demand::less_than(BUFF_SIZE);
        let w_x = ring.try_write(&w_demand);
        let mut w_segm = w_x.pick_left().unwrap();

        let msg = [1u8, 2u8, 4u8, 16u8];
        w_segm.move_items_from_as_buff(&msg);
    }
    {
        let r_demand = Demand::less_than(BUFF_SIZE);
        let r_x = ring.try_read(&r_demand);
        let mut r_segm = r_x.pick_left().unwrap();

        let mut msg = [MaybeUninit::<u8>::uninit(); BUFF_SIZE];
        let size = unsafe { r_segm.move_items_to_buff(&mut msg) };

        let vec: std::vec::Vec<u8> = msg
            .iter()
            .map(|m| unsafe { m.assume_init_read() })
            .collect();
        assert_eq!(
            &vec.as_slice()[..size],
            [1u8, 2u8, 4u8, 16u8].as_slice(),
        )
    }
}

dual_runtime_test_!(smoke_test_);
