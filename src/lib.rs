// to enable no hand-written poll
#![allow(unused_features)]
#![feature(async_fn_traits)]
#![feature(impl_trait_in_assoc_type)]
#![feature(unboxed_closures)]

#![no_std]
#![cfg_attr(test, feature(try_trait_v2))]

// We always pull in `std` during tests, because it's just easier
// to write tests when you can assume you're on a capable platform
#[cfg(test)]
extern crate std;

pub mod circular_buff;

pub mod x_deps {
    pub use abs_buff;
    pub use abs_buff::x_deps::{abs_cancel, anylr};
    pub use atomic_sync;
    pub use atomic_sync::x_deps::{abs_sync, atomex};
    pub use atomex::x_deps::funty;

    pub use mm_ptr;
    pub use mm_ptr::x_deps::abs_mm;
}
