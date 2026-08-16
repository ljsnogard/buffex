//! A tiny single-threaded executor used to prove that the ring buffer works
//! on any async runtime — including a hand-rolled one. It drives the
//! framework-agnostic futures (and the `futures_io` trait implementations)
//! without any external runtime dependency.

use std::{
    boxed::Box,
    future::Future,
    pin::Pin,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    task::{Context, Wake, Waker},
    vec::Vec,
};

struct MiniWaker(Arc<AtomicBool>);

impl Wake for MiniWaker {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A minimal cooperative executor: polls a list of tasks in a loop. Wakers
/// only set a flag; the executor re-polls everything when the flag is set.
pub(super) struct MiniExec {
    tasks: Vec<Pin<Box<dyn Future<Output = ()>>>>,
    wake: Arc<AtomicBool>,
}

impl MiniExec {
    pub fn new() -> Self {
        MiniExec {
            tasks: Vec::new(),
            wake: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn spawn(&mut self, f: impl Future<Output = ()> + 'static) {
        self.tasks.push(Box::pin(f));
    }

    /// Drive the executor until the `main` future completes.
    #[allow(dead_code)]
    pub fn block_on<T>(&mut self, main: impl Future<Output = T>) -> T {
        let mut main = Box::pin(main);
        loop {
            let waker = Waker::from(Arc::new(MiniWaker(self.wake.clone())));
            let mut cx = Context::from_waker(&waker);
            if let std::task::Poll::Ready(v) = main.as_mut().poll(&mut cx) {
                return v;
            }
            self.poll_tasks();
        }
    }

    /// Drive the executor until all spawned tasks complete.
    pub fn run_until_empty(&mut self) {
        loop {
            if self.tasks.is_empty() {
                return;
            }
            self.poll_tasks();
        }
    }

    fn poll_tasks(&mut self) {
        let waker = Waker::from(Arc::new(MiniWaker(self.wake.clone())));
        let mut cx = Context::from_waker(&waker);
        self.wake.store(false, Ordering::Relaxed);
        let mut i = 0;
        while i < self.tasks.len() {
            if self.tasks[i].as_mut().poll(&mut cx).is_ready() {
                let _ = self.tasks.swap_remove(i);
            } else {
                i += 1;
            }
        }
        if self.wake.load(Ordering::Relaxed) {
            return;
        }
        std::thread::yield_now();
    }
}
