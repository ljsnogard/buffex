#![no_std]

// The ring-buffer segment types implement `TrBuffSegmRef` / `TrBuffSegmMut`,
// whose signatures return `impl Try<...>`; the tests additionally use
// `core::ops::Try` (e.g. the cancellation token in `tests_/pipe_retry_`).
// Both need the `try_trait_v2` feature, so the flag applies to the whole
// crate, tests included.
#![feature(try_trait_v2)]

// We always pull in `std` during tests, because it's just easier
// to write tests when you can assume you're on a capable platform
#[cfg(test)]
extern crate std;

pub mod ring_buffer;

#[cfg(all(feature = "compio", unix))]
pub mod unix_stream;

pub mod x_deps {
    pub use abs_buff;
    pub use abs_buff::x_deps::{abs_cancel, anylr};
    pub use atomex;
    pub use atomex::x_deps::funty;
}
