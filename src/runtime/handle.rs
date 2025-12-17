use crate::runtime::scheduler::multi_thread::Shared;
use crate::task::{self, Task};
use crossbeam::deque::Injector;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Handle to the runtime.
///
/// Used to spawn tasks onto the runtime from outside it.
#[derive(Clone)]
pub struct Handle {
    /// Shared runtime state
    pub(crate) shared: Arc<Shared>,
    /// Global task queue
    pub(crate) injector: Arc<Injector<Task>>,
    /// Shutdown flag for current thread scheduler
    pub(crate) shutdown: Option<Arc<AtomicBool>>,
}

impl Handle {
    /// Spawn a new task onto the runtime.
    ///
    /// # Examples
    /// ```
    /// use tokio_like_runtime::Runtime;
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// let handle = rt.handle();
    ///
    /// handle.spawn(async {
    ///     println!("Hello from spawned task!");
    /// });
    /// ```
    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Create the task
        let (task, join_handle) = task::create_task(future, self.shared.clone());

        // Schedule it
        self.spawn_background(task);

        join_handle
    }

    /// Spawn a task that doesn't return a join handle.
    ///
    /// Useful for fire-and-forget tasks.
    pub fn spawn_background<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (task, _) = task::create_task(future, self.shared.clone());
        self.spawn_task(task);
    }

    /// Spawn a raw task (already created).
    pub(crate) fn spawn_task(&self, task: Task) {
        // Push to the global injector queue
        self.injector.push(task);

        // Wake up a worker if needed
        self.shared.notify_worker();
    }

    /// Returns true if the runtime is multi-threaded.
    pub fn is_multi_thread(&self) -> bool {
        !self.shared.stealers.lock().is_empty()
    }

    /// Returns true if the runtime has been shut down.
    pub fn is_shutdown(&self) -> bool {
        if let Some(shutdown) = &self.shutdown {
            shutdown.load(Ordering::Acquire)
        } else {
            self.shared.shutdown.load(Ordering::Acquire)
        }
    }

    /// Get a reference to the shared runtime state.
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Get a reference to the global task injector.
    pub fn injector(&self) -> &Arc<Injector<Task>> {
        &self.injector
    }

    /// Block on a future using this runtime handle.
    ///
    /// This runs the future on the current thread while allowing
    /// the runtime to process other tasks.
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Get the current thread's worker if it exists
        if let Some(mut worker) = self.try_get_worker() {
            // Use the worker thread to run the future
            self.block_on_with_worker(future, &mut worker)
        } else {
            // Fall back to simple blocking
            self.block_on_simple(future)
        }
    }

    /// Try to get a worker for the current thread.
    fn try_get_worker(&self) -> Option<WorkerRef> {
        use crate::runtime::scheduler::multi_thread::CURRENT_CONTEXT;

        CURRENT_CONTEXT.with(|cell| {
            cell.borrow().as_ref().map(|shared| {
                if Arc::ptr_eq(shared, &self.shared) {
                    WorkerRef::MultiThread
                } else {
                    WorkerRef::None
                }
            })
        })
        // .flatten()
    }

    /// Block on a future with a worker available.
    fn block_on_with_worker<F>(&self, future: F, _worker: &mut WorkerRef) -> F::Output
    where
        F: Future,
    {
        use parking_lot::Mutex;
        use std::task::{Context, Poll, Waker};

        struct BlockOnState<F> {
            future: std::pin::Pin<Box<F>>,
            waker: Option<Waker>,
            completed: bool,
        }

        let state = Arc::new(Mutex::new(BlockOnState {
            future: Box::pin(future),
            waker: None,
            completed: false,
        }));

        let waker = {
            let state = state.clone();
            unsafe fn raw_waker<F>(state: *const ()) -> std::task::RawWaker {
                use std::task::RawWakerVTable;

                fn clone<F>(data: *const ()) -> std::task::RawWaker {
                    raw_waker::<F>(data)
                }

                fn wake<F>(data: *const ()) {
                    let state = unsafe { Arc::from_raw(data as *const Mutex<BlockOnState<F>>) };
                    let mut lock = state.lock();
                    lock.completed = true;
                    if let Some(waker) = lock.waker.take() {
                        drop(lock);
                        waker.wake();
                    }
                }

                fn wake_by_ref<F>(data: *const ()) {
                    let state =
                        unsafe { (data as *const Mutex<BlockOnState<F>>).as_ref().unwrap() };
                    let mut lock = state.lock();
                    if let Some(waker) = lock.waker.take() {
                        drop(lock);
                        waker.wake();
                    }
                }

                fn drop_waker<F>(_data: *const ()) {}

                static VTABLE: RawWakerVTable =
                    RawWakerVTable::new(clone::<F>, wake::<F>, wake_by_ref::<F>, drop_waker::<F>);

                std::task::RawWaker::new(state as *const (), &VTABLE)
            }

            std::task::Waker::from_raw(unsafe {
                raw_waker::<F>(Arc::into_raw(state.clone()) as *const ())
            })
        };

        let mut cx = Context::from_waker(&waker);

        loop {
            let output = {
                let mut state = state.lock();
                if state.completed {
                    break;
                }

                match state.future.as_mut().poll(&mut cx) {
                    Poll::Ready(output) => Some(output),
                    Poll::Pending => None,
                }
            };

            if let Some(output) = output {
                return output;
            }

            // While waiting for the future to complete, process some tasks
            self.process_some_tasks();

            // Yield to avoid busy-waiting
            std::thread::yield_now();
        }

        unreachable!()
    }

    /// Simple blocking implementation without worker thread.
    fn block_on_simple<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        use std::task::{Context, Poll, Waker};

        struct SimpleWaker;

        impl std::task::Wake for SimpleWaker {
            fn wake(self: Arc<Self>) {
                // No-op for simple blocking
            }
        }

        let waker = Arc::new(SimpleWaker).into();
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);

        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    // Process some tasks while waiting
                    self.process_some_tasks();
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Process some available tasks.
    fn process_some_tasks(&self) {
        // Try to steal some work from the global queue
        for _ in 0..10 {
            if let Some(task) = self.injector.steal().success() {
                task.run();
            } else {
                break;
            }
        }
    }

    /// Enter the runtime context.
    ///
    /// This is used to mark that code is running within the runtime,
    /// which enables spawning tasks without an explicit handle.
    pub fn enter(&self) -> EnterGuard {
        use crate::runtime::scheduler::multi_thread::CURRENT_CONTEXT;

        let old = CURRENT_CONTEXT.with(|cell| cell.replace(Some(self.shared.clone())));

        EnterGuard {
            handle: self.clone(),
            old,
        }
    }

    /// Get the current runtime handle from thread-local storage.
    ///
    /// Returns `None` if called outside of a runtime context.
    pub fn current() -> Option<Self> {
        use crate::runtime::scheduler::multi_thread::CURRENT_CONTEXT;

        CURRENT_CONTEXT.with(|cell| {
            cell.borrow().as_ref().map(|shared| Handle {
                shared: shared.clone(),
                injector: shared.injector.clone(),
                shutdown: None,
            })
        })
    }
}

/// Guard that ensures the runtime context is restored when dropped.
pub struct EnterGuard {
    handle: Handle,
    old: Option<Arc<Shared>>,
}

impl Drop for EnterGuard {
    fn drop(&mut self) {
        use crate::runtime::scheduler::multi_thread::CURRENT_CONTEXT;

        CURRENT_CONTEXT.with(|cell| {
            *cell.borrow_mut() = self.old.take();
        });
    }
}

impl std::ops::Deref for EnterGuard {
    type Target = Handle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

/// Reference to a worker thread.
enum WorkerRef {
    MultiThread,
    CurrentThread,
    None,
}

// // Implementations for current thread scheduler handle
// impl Handle {
//     /// Create a handle for a current thread scheduler.
//     pub(crate) fn for_current_thread(
//         injector: Arc<Injector<Task>>,
//         shutdown: Arc<AtomicBool>,
//     ) -> Self {
//         use crate::runtime::scheduler::current_thread::TASK_QUEUE;

//         // For current thread, we need a different shared type
//         // We'll create a dummy shared that just holds the queue
//         struct CurrentThreadShared {
//             queue: std::sync::Mutex<Option<std::collections::VecDeque<Task>>>,
//         }

//         let shared = Arc::new(Shared {
//             injector: injector.clone(),
//             stealers: parking_lot::Mutex::new(Vec::new()),
//             active: std::sync::atomic::AtomicUsize::new(1),
//             shutdown: AtomicBool::new(false),
//             condvar: std::sync::Condvar::new(),
//             mutex: std::sync::Mutex::new(()),
//         });

//         Handle {
//             shared,
//             injector,
//             shutdown: Some(shutdown),
//         }
//     }
// }

// Extension trait for easier spawning
pub trait RuntimeExt {
    /// Spawn a task from a future that returns `()`.
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;

    /// Spawn a task and get a join handle.
    fn spawn_join<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;
}

impl RuntimeExt for Handle {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_background(future);
    }

    fn spawn_join<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn(future)
    }
}

// Helper function for spawning from anywhere
pub fn spawn<F>(future: F) -> crate::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if let Some(handle) = Handle::current() {
        handle.spawn(future)
    } else {
        panic!(
            "no runtime handle available. Call this from within a runtime context or provide an explicit handle."
        )
    }
}

// // Spawn tasks using a handle
// let handle = rt.handle();
// let task1 = handle.spawn(async {
//     println!("Task 1");
// });

// // Spawn without explicit handle (requires runtime context)
// let guard = handle.enter();
// let task2 = crate::task::spawn(async {
//     println!("Task 2");
// });
// drop(guard);

// // Block on a future using the handle
// let result = handle.block_on(async {
//     42
// });
