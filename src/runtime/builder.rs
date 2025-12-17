use super::{CurrentThread, MultiThread};
use std::io;

pub struct Builder {
    worker_threads: usize,
    flavor: RuntimeFlavor,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeFlavor {
    CurrentThread,
    MultiThread,
}

impl Builder {
    pub fn new_multi_thread() -> Self {
        Self {
            worker_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            flavor: RuntimeFlavor::MultiThread,
        }
    }

    pub fn new_current_thread() -> Self {
        Self {
            worker_threads: 1,
            flavor: RuntimeFlavor::CurrentThread,
        }
    }

    pub fn worker_threads(mut self, val: usize) -> Self {
        self.worker_threads = val.max(1);
        self
    }

    pub fn build(self) -> io::Result<super::Runtime> {
        match self.flavor {
            RuntimeFlavor::MultiThread => {
                let scheduler = MultiThread::new(self.worker_threads)?;
                Ok(super::Runtime {
                    inner: super::RuntimeKind::MultiThread(scheduler),
                })
            }
            RuntimeFlavor::CurrentThread => {
                let scheduler = CurrentThread::new()?;
                Ok(super::Runtime {
                    inner: super::RuntimeKind::CurrentThread(scheduler),
                })
            }
        }
    }
}
