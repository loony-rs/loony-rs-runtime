mod raw;
mod waker;

use crate::runtime::scheduler::multi_thread::Shared;
use raw::{Header, Task as RawTask, TaskState, create_raw_task};
use std::future::Future;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

pub use raw::Task;
pub use waker::waker_from_task;

/// A handle that can be used to await the result of a spawned task.
pub struct JoinHandle<T> {
    header: NonNull<Header>,
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    /// Creates a new join handle from a raw task header.
    pub(crate) fn new(header: NonNull<Header>) -> Self {
        Self {
            header,
            _marker: PhantomData,
        }
    }

    /// Abort the task, preventing it from executing further.
    pub fn abort(&self) {
        unsafe {
            raw::abort_task(self.header);
        }
    }

    /// Returns true if the task has completed.
    pub fn is_finished(&self) -> bool {
        unsafe {
            let state = (*self.header.as_ptr())
                .state
                .load(std::sync::atomic::Ordering::Acquire);
            TaskState::from_bits(state).is_complete()
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let header = self.header.as_ptr();
            let state =
                TaskState::from_bits((*header).state.load(std::sync::atomic::Ordering::Acquire));

            if state.is_complete() {
                // Task is complete, try to read the output
                if let Some(output) = raw::try_read_output::<T>(self.header) {
                    Poll::Ready(Ok(output))
                } else if state.is_aborted() {
                    Poll::Ready(Err(JoinError::Aborted))
                } else {
                    // Output was already taken
                    Poll::Ready(Err(JoinError::AlreadyTaken))
                }
            } else if state.is_aborted() {
                // Task was aborted
                Poll::Ready(Err(JoinError::Aborted))
            } else {
                // Task is still running, register interest
                if raw::set_join_waker(self.header, cx.waker().clone()) {
                    // Task completed between check and setting waker
                    self.poll(cx)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // When a JoinHandle is dropped, we need to ensure the task gets cleaned up
        // if it's complete and there are no other references.
        unsafe {
            let state = (*self.header.as_ptr())
                .state
                .load(std::sync::atomic::Ordering::Acquire);
            let task_state = TaskState::from_bits(state);

            if task_state.is_complete() {
                // Clear the JOIN_INTEREST flag since we're no longer interested
                (*self.header.as_ptr()).state.fetch_and(
                    !TaskState::JOIN_INTEREST.bits(),
                    std::sync::atomic::Ordering::AcqRel,
                );

                // Clear any stored waker
                (*self.header.as_ptr()).join_waker.get().take();
            }
        }
    }
}

/// Error returned when awaiting a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// The task was aborted before completion.
    Aborted,
    /// The task panicked during execution.
    Panic,
    /// The task's output was already taken by another join handle.
    AlreadyTaken,
}

/// Spawn a new task onto the current runtime.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = crate::runtime::Handle::current()
        .expect("no runtime active. Call this from within a runtime context.");

    handle.spawn(future)
}

/// Spawn a task that is !Send (must run on the current thread).
pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    // Get current thread's scheduler
    use crate::runtime::scheduler::current_thread;

    let (task, join_handle) = create_task(future, Arc::new(()));

    current_thread::spawn_local_task(task);

    join_handle
}

/// Create a new task from a future.
pub(crate) fn create_task<F, S>(future: F, scheduler: Arc<S>) -> (Task, JoinHandle<F::Output>)
where
    F: Future + 'static,
    F::Output: 'static,
    S: Fn(*const Header) + Send + Sync + 'static,
{
    // Create scheduler closure
    let scheduler_clone = scheduler.clone();
    let schedule_fn = move |header_ptr: *const Header| {
        let task = unsafe { Task::from_raw(NonNull::new_unchecked(header_ptr as *mut Header)) };

        // Get current runtime handle
        if let Some(handle) = crate::runtime::Handle::current() {
            handle.spawn_task(task);
        } else {
            // Fallback: run on current thread
            task.run();
        }
    };

    // Create the raw task
    let (raw_task, header) = create_raw_task(future, schedule_fn);

    (raw_task, JoinHandle::new(header))
}

/// Create a task using a shared scheduler.
pub(crate) fn create_task_with_shared<F>(
    future: F,
    shared: Arc<Shared>,
) -> (Task, JoinHandle<F::Output>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let shared_clone = shared.clone();
    let schedule_fn = move |header_ptr: *const Header| {
        let task = unsafe { Task::from_raw(NonNull::new_unchecked(header_ptr as *mut Header)) };

        // Push to the global queue and notify a worker
        shared_clone.injector.push(task);
        shared_clone.notify_worker();
    };

    let (raw_task, header) = create_raw_task(future, schedule_fn);

    (raw_task, JoinHandle::new(header))
}

/// Yield execution to the runtime, allowing other tasks to run.
pub async fn yield_now() {
    struct YieldNow {
        yielded: bool,
    }

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if !self.yielded {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }

    YieldNow { yielded: false }.await
}

/// Run a future to completion on the current thread, without blocking.
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{RawWaker, RawWakerVTable};

    // Create a simple waker that does nothing
    unsafe fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }

    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    unsafe fn noop_raw_waker() -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                // In a real runtime, this would process pending tasks
                std::thread::yield_now();
            }
        }
    }
}

/// Extension trait for futures to spawn them as tasks.
pub trait FutureExt: Future + Sized {
    /// Spawn this future as a new task.
    fn spawn(self) -> JoinHandle<Self::Output>
    where
        Self: Send + 'static,
        Self::Output: Send + 'static,
    {
        spawn(self)
    }

    /// Spawn this future as a local task (must run on current thread).
    fn spawn_local(self) -> JoinHandle<Self::Output>
    where
        Self: 'static,
        Self::Output: 'static,
    {
        spawn_local(self)
    }
}

impl<F: Future> FutureExt for F {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_task_creation() {
        let future = async { 42 };
        let (task, handle) = create_task(future, Arc::new(|_| {}));

        // Task should be initially idle
        assert!(task.state().is_idle());

        // Run the task
        task.run();

        // Task should now be complete
        assert!(task.state().is_complete());

        // Join handle should return the result
        let result = block_on(handle);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_task_abort() {
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let future = async move {
            completed_clone.store(true, Ordering::SeqCst);
            42
        };

        let (task, handle) = create_task(future, Arc::new(|_| {}));

        // Abort the task before running
        handle.abort();

        // Run the task (should not execute due to abort)
        task.run();

        // Task should be aborted
        assert!(task.state().is_aborted());

        // Future should not have executed
        assert!(!completed.load(Ordering::SeqCst));

        // Join handle should return Aborted error
        let result = block_on(handle);
        assert!(matches!(result, Err(JoinError::Aborted)));
    }

    #[test]
    fn test_yield_now() {
        let mut count = 0;
        let future = async {
            count += 1;
            yield_now().await;
            count += 1;
            yield_now().await;
            count += 1;
        };

        block_on(future);
        assert_eq!(count, 3);
    }
}
