use crate::task::{self, Task};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

thread_local! {
    static TASK_QUEUE: RefCell<VecDeque<Task>> = RefCell::new(VecDeque::new());
    static PARKER: RefCell<Option<Arc<ThreadParker>>> = RefCell::new(None);
}

pub struct CurrentThread {
    handle: crate::runtime::Handle,
    parker: Arc<ThreadParker>,
    shutdown: Arc<AtomicBool>,
}

impl CurrentThread {
    pub fn new() -> io::Result<Self> {
        let parker = Arc::new(ThreadParker::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        PARKER.with(|cell| {
            *cell.borrow_mut() = Some(parker.clone());
        });

        let handle = crate::runtime::Handle::for_current_thread(
            Arc::new(crossbeam::deque::Injector::new()),
            shutdown.clone(),
        );

        Ok(Self {
            handle,
            parker,
            shutdown,
        })
    }

    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        let waker = self.parker.waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);

        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    self.run_until_idle();

                    if self.is_idle() {
                        self.parker.park();
                    }
                }
            }
        }
    }

    fn run_until_idle(&self) {
        TASK_QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();

            while let Some(task) = queue.pop_front() {
                task.run();
            }
        });
    }

    fn is_idle(&self) -> bool {
        TASK_QUEUE.with(|queue| queue.borrow().is_empty())
    }

    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (task, handle) = task::create_task(future, Arc::new(()));

        TASK_QUEUE.with(|queue| {
            queue.borrow_mut().push_back(task);
        });

        // Wake up the parker if it's sleeping
        self.parker.unpark();

        handle
    }

    pub(crate) fn spawn_task(&self, task: Task) {
        TASK_QUEUE.with(|queue| {
            queue.borrow_mut().push_back(task);
        });

        self.parker.unpark();
    }

    pub fn handle(&self) -> crate::runtime::Handle {
        self.handle.clone()
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.parker.unpark();

        TASK_QUEUE.with(|queue| {
            queue.borrow_mut().clear();
        });

        PARKER.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }

    pub fn turn(&mut self, duration: Option<std::time::Duration>) -> bool {
        self.run_until_idle();

        if self.is_idle() && !self.shutdown.load(Ordering::Acquire) {
            if let Some(duration) = duration {
                self.parker.park_timeout(duration);
            } else {
                self.parker.park();
            }
        }

        !self.shutdown.load(Ordering::Acquire)
    }
}

struct ThreadParker {
    inner: Arc<ThreadParkerInner>,
}

struct ThreadParkerInner {
    state: std::sync::atomic::AtomicUsize,
    condvar: std::sync::Condvar,
    mutex: std::sync::Mutex<()>,
}

const EMPTY: usize = 0;
const PARKED: usize = 1;
const NOTIFIED: usize = 2;

impl ThreadParker {
    fn new() -> Self {
        Self {
            inner: Arc::new(ThreadParkerInner {
                state: std::sync::atomic::AtomicUsize::new(EMPTY),
                condvar: std::sync::Condvar::new(),
                mutex: std::sync::Mutex::new(()),
            }),
        }
    }

    fn park(&self) {
        self.park_timeout(None);
    }

    fn park_timeout(&self, duration: Option<std::time::Duration>) {
        let mut state = self.inner.state.load(std::sync::atomic::Ordering::Acquire);

        loop {
            if state == NOTIFIED {
                match self.inner.state.compare_exchange(
                    NOTIFIED,
                    EMPTY,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(s) => state = s,
                }
                continue;
            }

            match self.inner.state.compare_exchange(
                EMPTY,
                PARKED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(s) => state = s,
            }
        }

        let guard = self.inner.mutex.lock().unwrap();

        if let Some(duration) = duration {
            let (_, timeout_result) = self.inner.condvar.wait_timeout(guard, duration).unwrap();
            if timeout_result.timed_out()
                && self.inner.state.load(std::sync::atomic::Ordering::Acquire) == PARKED
            {
                // Timeout occurred
                self.inner
                    .state
                    .store(EMPTY, std::sync::atomic::Ordering::Release);
                return;
            }
        } else {
            let _ = self.inner.condvar.wait(guard).unwrap();
        }

        match self
            .inner
            .state
            .swap(EMPTY, std::sync::atomic::Ordering::AcqRel)
        {
            NOTIFIED => (),
            PARKED => (),
            _ => unreachable!(),
        }
    }

    fn unpark(&self) {
        let state = self
            .inner
            .state
            .swap(NOTIFIED, std::sync::atomic::Ordering::AcqRel);

        if state == PARKED {
            let _guard = self.inner.mutex.lock().unwrap();
            self.inner.condvar.notify_one();
        }
    }

    fn waker(&self) -> Waker {
        use std::task::{RawWaker, RawWakerVTable};

        unsafe fn clone(data: *const ()) -> RawWaker {
            let inner = Arc::from_raw(data as *const ThreadParkerInner);
            let clone = inner.clone();
            std::mem::forget(inner);
            std::mem::forget(clone.clone());
            RawWaker::new(Arc::into_raw(clone) as *const (), &VTABLE)
        }

        unsafe fn wake(data: *const ()) {
            let inner = Arc::from_raw(data as *const ThreadParkerInner);
            let parker = ThreadParker { inner };
            parker.unpark();
        }

        unsafe fn wake_by_ref(data: *const ()) {
            let inner = Arc::from_raw(data as *const ThreadParkerInner);
            let parker = ThreadParker {
                inner: inner.clone(),
            };
            parker.unpark();
            std::mem::forget(inner);
        }

        unsafe fn drop_waker(data: *const ()) {
            let _ = Arc::from_raw(data as *const ThreadParkerInner);
        }

        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

        let raw = RawWaker::new(Arc::into_raw(self.inner.clone()) as *const (), &VTABLE);

        unsafe { Waker::from_raw(raw) }
    }
}

// Handle for current thread
impl crate::runtime::Handle {
    pub(crate) fn for_current_thread(
        injector: Arc<crossbeam::deque::Injector<Task>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        // For current thread scheduler, we create a dummy shared state
        struct CurrentThreadShared;

        let shared = Arc::new(crate::runtime::scheduler::multi_thread::Shared {
            injector: injector.clone(),
            stealers: parking_lot::Mutex::new(Vec::new()),
            unparkers: parking_lot::Mutex::new(Vec::new()),
            active: std::sync::atomic::AtomicUsize::new(1),
            shutdown: AtomicBool::new(false),
            condvar: std::sync::Condvar::new(),
            mutex: std::sync::Mutex::new(()),
        });

        Self {
            shared,
            injector,
            shutdown: Some(shutdown),
        }
    }
}

// Thread-local spawn function
pub(crate) fn spawn_local<F>(future: F) -> crate::task::JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (task, handle) = task::create_task(future, Arc::new(()));

    TASK_QUEUE.with(|queue| {
        queue.borrow_mut().push_back(task);
    });

    // Wake up the parker if sleeping
    PARKER.with(|cell| {
        if let Some(parker) = cell.borrow().as_ref() {
            parker.unpark();
        }
    });

    handle
}
