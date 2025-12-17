mod builder;
mod handle;
mod park;
mod scheduler;

use std::future::Future;
use std::io;

pub use builder::Builder;
pub use handle::Handle;
pub use scheduler::{current_thread::CurrentThread, multi_thread::MultiThread};

pub struct Runtime {
    inner: RuntimeKind,
}

enum RuntimeKind {
    CurrentThread(CurrentThread),
    MultiThread(MultiThread),
}

impl Runtime {
    pub fn new() -> io::Result<Self> {
        Builder::new_multi_thread().build()
    }

    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        match &mut self.inner {
            RuntimeKind::CurrentThread(rt) => rt.block_on(future),
            RuntimeKind::MultiThread(rt) => rt.block_on(future),
        }
    }

    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match &self.inner {
            RuntimeKind::CurrentThread(rt) => rt.spawn(future),
            RuntimeKind::MultiThread(rt) => rt.spawn(future),
        }
    }

    pub fn handle(&self) -> Handle {
        match &self.inner {
            RuntimeKind::CurrentThread(rt) => rt.handle(),
            RuntimeKind::MultiThread(rt) => rt.handle(),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        match &mut self.inner {
            RuntimeKind::CurrentThread(rt) => rt.shutdown(),
            RuntimeKind::MultiThread(rt) => rt.shutdown(),
        }
    }
}
