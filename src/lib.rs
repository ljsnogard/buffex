#![no_std]

// We always pull in `std` during tests, because it's just easier
// to write tests when you can assume you're on a capable platform
#[cfg(test)]
extern crate std;

pub mod ring_buffer;

pub mod x_deps {
    pub use abs_buff;
    pub use asyncex;

    pub use asyncex::x_deps::{abs_sync, atomex, atomic_sync};
}
