use crate::task::{self, Task};
use crossbeam::deque::{Injector, Steal, Stealer, Worker};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar};
use std::thread;
use std::time::Duration;

thread_local! {
    static CURRENT_CONTEXT: RefCell<Option<Arc<Shared>>> = RefCell::new(None);
    static CURRENT_WORKER: RefCell<Option<WorkerRef>> = RefCell::new(None);
}

struct WorkerThread {
    worker: Worker<Task>,
    index: usize,
    shared: Arc<Shared>,
}

pub struct Shared {
    pub(crate) injector: Injector<Task>,
    pub(crate) stealers: Mutex<Vec<Stealer<Task>>>,
    pub(crate) unparkers: Mutex<Vec<Arc<ThreadUnpark>>>,
    pub(crate) active: AtomicUsize,
    pub(crate) shutdown: AtomicBool,
    pub(crate) condvar: Condvar,
    pub(crate) mutex: std::sync::Mutex<()>,
}

pub struct MultiThread {
    shared: Arc<Shared>,
    handle: crate::runtime::Handle,
    workers: Vec<thread::JoinHandle<()>>,
}

impl MultiThread {
    pub fn new(num_threads: usize) -> io::Result<Self> {
        let mut workers = Vec::with_capacity(num_threads);
        let mut stealers = Vec::with_capacity(num_threads);
        let mut unparkers = Vec::with_capacity(num_threads);

        for i in 0..num_threads {
            let worker = Worker::new_fifo();
            let stealer = worker.stealer();
            stealers.push(stealer);
            workers.push(worker);

            let unparker = Arc::new(ThreadUnpark {
                thread: Mutex::new(None),
                id: i,
            });
            unparkers.push(unparker);
        }

        let shared = Arc::new(Shared {
            injector: Injector::new(),
            stealers: Mutex::new(stealers),
            unparkers: Mutex::new(unparkers),
            active: AtomicUsize::new(num_threads),
            shutdown: AtomicBool::new(false),
            condvar: Condvar::new(),
            mutex: std::sync::Mutex::new(()),
        });

        let mut worker_handles = Vec::with_capacity(num_threads);

        for (index, worker) in workers.into_iter().enumerate() {
            let worker_thread = WorkerThread {
                worker,
                index,
                shared: shared.clone(),
            };

            let handle = thread::Builder::new()
                .name(format!("worker-{}", index))
                .spawn(move || worker_thread.run())?;

            worker_handles.push(handle);
        }

        let handle = crate::runtime::Handle {
            shared: shared.clone(),
            injector: shared.injector.clone(),
            shutdown: None,
        };

        Ok(MultiThread {
            shared,
            handle,
            workers: worker_handles,
        })
    }

    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        // Create a dedicated parker for block_on
        struct BlockOnParker {
            thread: thread::Thread,
            unparked: std::sync::atomic::AtomicBool,
        }

        impl BlockOnParker {
            fn new() -> Self {
                Self {
                    thread: thread::current(),
                    unparked: std::sync::atomic::AtomicBool::new(false),
                }
            }

            fn park(&self) {
                while !self.unparked.swap(false, Ordering::Acquire) {
                    thread::park();
                }
            }

            fn unpark(&self) {
                if !self.unparked.swap(true, Ordering::Release) {
                    self.thread.unpark();
                }
            }

            fn waker(&self) -> std::task::Waker {
                use std::task::{RawWaker, RawWakerVTable};

                unsafe fn clone(data: *const ()) -> RawWaker {
                    RawWaker::new(data, &VTABLE)
                }

                unsafe fn wake(data: *const ()) {
                    let parker = &*(data as *const BlockOnParker);
                    parker.unpark();
                }

                unsafe fn wake_by_ref(data: *const ()) {
                    let parker = &*(data as *const BlockOnParker);
                    parker.unpark();
                }

                unsafe fn drop_waker(_: *const ()) {}

                static VTABLE: RawWakerVTable =
                    RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

                unsafe {
                    std::task::Waker::from_raw(RawWaker::new(
                        self as *const _ as *const (),
                        &VTABLE,
                    ))
                }
            }
        }

        let parker = BlockOnParker::new();
        let waker = parker.waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);

        // Set thread-local context for this block_on call
        let _guard = ContextGuard::new(self.shared.clone());

        loop {
            match future.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => {
                    // While waiting, help process tasks
                    if let Some(task) = self.steal_work() {
                        task.run();
                    } else if self.shared.injector.is_empty() {
                        // No work available, park
                        parker.park();
                    } else {
                        // Yield and try again
                        thread::yield_now();
                    }
                }
            }
        }
    }

    fn steal_work(&self) -> Option<Task> {
        let stealers = self.shared.stealers.lock();

        // First try to steal from other workers
        for stealer in stealers.iter() {
            match stealer.steal() {
                Steal::Success(task) => return Some(task),
                Steal::Empty | Steal::Retry => continue,
            }
        }

        // Then try the global queue
        self.shared.injector.steal().success()
    }

    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (task, handle) = task::create_task(future, self.shared.clone());
        self.shared.injector.push(task);

        // Notify a worker
        self.shared.notify_worker();

        handle
    }

    pub fn handle(&self) -> crate::runtime::Handle {
        self.handle.clone()
    }

    pub fn shutdown(&mut self) {
        // Set shutdown flag
        self.shared.shutdown.store(true, Ordering::Release);

        // Wake up all workers
        {
            let _lock = self.shared.mutex.lock().unwrap();
            self.shared.condvar.notify_all();
        }

        // Unpark all worker threads
        let unparkers = self.shared.unparkers.lock();
        for unparker in unparkers.iter() {
            unparker.unpark();
        }

        // Wait for all workers to finish
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }

        // Clear thread-local state
        CURRENT_CONTEXT.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }

    pub(crate) fn notify_worker(&self) {
        // Try to find a sleeping worker to wake up
        let unparkers = self.shared.unparkers.lock();
        if let Some(unparker) = unparkers.first() {
            unparker.unpark();
        }
    }
}

impl WorkerThread {
    fn run(self) {
        // Set thread-local context
        CURRENT_CONTEXT.with(|cell| {
            *cell.borrow_mut() = Some(self.shared.clone());
        });

        // Create worker reference
        let worker_ref = WorkerRef {
            index: self.index,
            worker: self.worker,
        };

        CURRENT_WORKER.with(|cell| {
            *cell.borrow_mut() = Some(worker_ref.clone());
        });

        let WorkerThread {
            mut worker,
            index,
            shared,
        } = self;

        while !shared.shutdown.load(Ordering::Acquire) {
            // Try to pop from local queue
            if let Some(task) = worker.pop() {
                task.run();
                continue;
            }

            // Try to steal from other workers
            if let Some(task) = Self::steal_work(&shared, index, &worker) {
                task.run();
                continue;
            }

            // Try the global queue
            if let Some(task) = shared.injector.steal().success() {
                task.run();
                continue;
            }

            // No work available, go to sleep
            let mut lock = shared.mutex.lock().unwrap();
            shared.active.fetch_sub(1, Ordering::AcqRel);

            // Double-check for work before sleeping
            if worker.pop().is_some() || shared.injector.steal().success().is_some() {
                shared.active.fetch_add(1, Ordering::AcqRel);
                continue;
            }

            while !shared.shutdown.load(Ordering::Acquire) {
                // Wait for work or shutdown
                shared.condvar.wait(&mut lock);

                // Check for work again
                if worker.pop().is_some() || shared.injector.steal().success().is_some() {
                    break;
                }
            }

            shared.active.fetch_add(1, Ordering::AcqRel);
        }

        // Clear thread-local state
        CURRENT_CONTEXT.with(|cell| {
            *cell.borrow_mut() = None;
        });

        CURRENT_WORKER.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }

    fn steal_work(shared: &Arc<Shared>, my_index: usize, my_worker: &Worker<Task>) -> Option<Task> {
        let stealers = shared.stealers.lock();
        let num_workers = stealers.len();

        // Start stealing from the next worker
        let start = (my_index + 1) % num_workers;

        for offset in 0..num_workers - 1 {
            let target = (start + offset) % num_workers;
            match stealers[target].steal() {
                Steal::Success(task) => return Some(task),
                Steal::Empty | Steal::Retry => continue,
            }
        }

        None
    }
}

#[derive(Clone)]
struct WorkerRef {
    index: usize,
    worker: Worker<Task>,
}

struct ThreadUnpark {
    thread: Mutex<Option<thread::Thread>>,
    id: usize,
}

impl ThreadUnpark {
    fn park(&self) {
        let mut thread_guard = self.thread.lock();
        *thread_guard = Some(thread::current());
    }

    fn unpark(&self) {
        let thread_guard = self.thread.lock();
        if let Some(thread) = thread_guard.as_ref() {
            thread.unpark();
        }
    }
}

struct ContextGuard {
    old_context: Option<Arc<Shared>>,
    old_worker: Option<WorkerRef>,
}

impl ContextGuard {
    fn new(context: Arc<Shared>) -> Self {
        let old_context = CURRENT_CONTEXT.with(|cell| cell.replace(Some(context)));

        let old_worker = CURRENT_WORKER.with(|cell| cell.replace(None));

        Self {
            old_context,
            old_worker,
        }
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CURRENT_CONTEXT.with(|cell| {
            *cell.borrow_mut() = self.old_context.take();
        });

        CURRENT_WORKER.with(|cell| {
            *cell.borrow_mut() = self.old_worker.take();
        });
    }
}

// Helper functions
pub(crate) fn current_shared() -> Option<Arc<Shared>> {
    CURRENT_CONTEXT.with(|cell| cell.borrow().clone())
}

pub(crate) fn current_worker() -> Option<WorkerRef> {
    CURRENT_WORKER.with(|cell| cell.borrow().clone())
}
