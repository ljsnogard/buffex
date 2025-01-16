#![no_std]

#![feature(unboxed_closures)] // To allow a struct implement Fn*
#![feature(fn_traits)]

// We always pull in `std` during tests, because it's just easier
// to write tests when you can assume you're on a capable platform
#[cfg(test)]
extern crate std;

pub mod ring_buffer;

pub mod x_deps {
    pub use atomex;
    pub use atomex::x_deps::funty;

    pub use segm_buff;
    pub use segm_buff::x_deps::{abs_buff, abs_sync};

    pub use smallvec;
}
