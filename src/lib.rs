#![no_std]

#![feature(min_specialization)] // To allow specialization for `Clone` and `Copy`
#![feature(unboxed_closures)] // To allow a struct implement Fn*
#![feature(fn_traits)]

// We always pull in `std` during tests, because it's just easier
// to write tests when you can assume you're on a capable platform
#[cfg(test)]
extern crate std;

pub mod ring_buffer;
pub mod slices;

pub mod x_deps {
    pub use abs_buff;
    pub use abs_buff::x_deps::abs_sync;

    pub use atomex;
    pub use atomex::x_deps::funty;

    pub use smallvec;
}
