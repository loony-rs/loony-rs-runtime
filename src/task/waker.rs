use crate::task::raw::{Header, Task, TaskState};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::task::{RawWaker, RawWakerVTable, Waker};

/// Create a waker from a task header
pub(crate) fn waker_from_task(header: NonNull<Header>) -> Waker {
    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        let header = NonNull::new_unchecked(data as *mut Header);

        // Increment the reference count
        let prev_state = (*header.as_ptr())
            .state
            .fetch_add(1 << TaskState::REFCOUNT_SHIFT, Ordering::Relaxed);

        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn wake_waker(data: *const ()) {
        let header = NonNull::new_unchecked(data as *mut Header);

        // Try to transition from IDLE to SCHEDULED
        loop {
            let current = (*header.as_ptr()).state.load(Ordering::Acquire);
            let state = TaskState::from_bits(current);

            // Check if task is already complete or aborted
            if state.is_complete() || state.is_aborted() {
                break;
            }

            // Check if already scheduled or running
            if state.is_scheduled() || state.is_running() {
                break;
            }

            // Only idle tasks can be woken
            if !state.is_idle() {
                break;
            }

            // Try to transition from IDLE to SCHEDULED
            let new_state = TaskState::SCHEDULED.bits();
            let desired = (current & !TaskState::STATE_MASK) | new_state;

            match (*header.as_ptr()).state.compare_exchange_weak(
                current,
                desired,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully scheduled, wake the task
                    let task = Task { header };
                    task.schedule();
                    break;
                }
                Err(_) => {
                    // State changed, retry
                    continue;
                }
            }
        }
    }

    unsafe fn wake_by_ref_waker(data: *const ()) {
        let header = NonNull::new_unchecked(data as *mut Header);

        // Try to transition from IDLE to SCHEDULED
        loop {
            let current = (*header.as_ptr()).state.load(Ordering::Acquire);
            let state = TaskState::from_bits(current);

            // Check if task is already complete or aborted
            if state.is_complete() || state.is_aborted() {
                break;
            }

            // Check if already scheduled or running
            if state.is_scheduled() || state.is_running() {
                break;
            }

            // Only idle tasks can be woken
            if !state.is_idle() {
                break;
            }

            // Try to transition from IDLE to SCHEDULED
            let new_state = TaskState::SCHEDULED.bits();
            let desired = (current & !TaskState::STATE_MASK) | new_state;

            match (*header.as_ptr()).state.compare_exchange_weak(
                current,
                desired,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully scheduled, wake the task
                    let task = Task { header };
                    task.schedule();
                    break;
                }
                Err(_) => {
                    // State changed, retry
                    continue;
                }
            }
        }
    }

    unsafe fn drop_waker(data: *const ()) {
        let header = NonNull::new_unchecked(data as *mut Header);

        // Decrement the reference count
        let prev_state = (*header.as_ptr())
            .state
            .fetch_sub(1 << TaskState::REFCOUNT_SHIFT, Ordering::Release);

        let prev = TaskState::from_bits(prev_state);

        // If this was the last waker and task is complete, deallocate
        if prev.refcount() == 1 && prev.is_complete() {
            let vtable = (*header.as_ptr()).vtable;
            (vtable.deallocate)(header);
        }
    }

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_waker, wake_waker, wake_by_ref_waker, drop_waker);

    let ptr = header.as_ptr() as *const ();
    unsafe { Waker::from_raw(RawWaker::new(ptr, &WAKER_VTABLE)) }
}

/// Create a no-op waker that does nothing
pub(crate) fn noop_waker() -> Waker {
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

    unsafe { Waker::from_raw(noop_raw_waker()) }
}

/// Create a waker that panics when woken
#[cfg(test)]
pub(crate) fn panic_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        panic_raw_waker()
    }

    unsafe fn wake(_: *const ()) {
        panic!("panic_waker was woken");
    }

    unsafe fn wake_by_ref(_: *const ()) {
        panic!("panic_waker was woken by ref");
    }

    unsafe fn drop(_: *const ()) {}

    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    unsafe fn panic_raw_waker() -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }

    unsafe { Waker::from_raw(panic_raw_waker()) }
}

/// A waker that can be used to manually wake a task
pub struct ManualWaker {
    header: Option<NonNull<Header>>,
}

impl ManualWaker {
    /// Create a new manual waker for a task
    pub fn new(header: NonNull<Header>) -> Self {
        // Increment refcount
        unsafe {
            (*header.as_ptr())
                .state
                .fetch_add(1 << TaskState::REFCOUNT_SHIFT, Ordering::Relaxed);
        }

        Self {
            header: Some(header),
        }
    }

    /// Wake the task
    pub fn wake(&self) {
        if let Some(header) = self.header {
            unsafe {
                // Try to schedule the task
                let task = Task { header };
                task.schedule();
            }
        }
    }

    /// Convert to a standard waker
    pub fn into_waker(self) -> Waker {
        if let Some(header) = self.header {
            waker_from_task(header)
        } else {
            noop_waker()
        }
    }
}

impl Drop for ManualWaker {
    fn drop(&mut self) {
        if let Some(header) = self.header.take() {
            // Decrement the reference count
            let prev_state = unsafe {
                (*header.as_ptr())
                    .state
                    .fetch_sub(1 << TaskState::REFCOUNT_SHIFT, Ordering::Release)
            };

            let prev = TaskState::from_bits(prev_state);

            // If this was the last waker and task is complete, deallocate
            if prev.refcount() == 1 && prev.is_complete() {
                let vtable = unsafe { (*header.as_ptr()).vtable };
                unsafe {
                    (vtable.deallocate)(header);
                }
            }
        }
    }
}

/// A waker that calls a closure when woken
pub struct CallbackWaker<F>
where
    F: Fn() + Send + Sync + 'static,
{
    callback: Option<Box<F>>,
}

impl<F> CallbackWaker<F>
where
    F: Fn() + Send + Sync + 'static,
{
    /// Create a new callback waker
    pub fn new(callback: F) -> Self {
        Self {
            callback: Some(Box::new(callback)),
        }
    }

    /// Convert to a standard waker
    pub fn into_waker(self) -> Waker {
        use std::task::{RawWaker, RawWakerVTable};

        unsafe fn clone<F: Fn() + Send + Sync + 'static>(data: *const ()) -> RawWaker {
            let callback = data as *const CallbackWaker<F>;
            let callback_ref = &*callback;

            // Increment refcount (using Box leak as reference count)
            let cloned = Box::new(CallbackWaker {
                callback: callback_ref.callback.clone(),
            });

            RawWaker::new(
                Box::into_raw(cloned) as *const (),
                &CALLBACK_WAKER_VTABLE::<F>,
            )
        }

        unsafe fn wake<F: Fn() + Send + Sync + 'static>(data: *const ()) {
            let callback = &*(data as *const CallbackWaker<F>);
            if let Some(cb) = &callback.callback {
                cb();
            }
        }

        unsafe fn wake_by_ref<F: Fn() + Send + Sync + 'static>(data: *const ()) {
            wake::<F>(data);
        }

        unsafe fn drop_waker<F: Fn() + Send + Sync + 'static>(data: *const ()) {
            let _ = Box::from_raw(data as *mut CallbackWaker<F>);
        }

        struct CALLBACK_WAKER_VTABLE<F>(std::marker::PhantomData<F>);

        impl<F: Fn() + Send + Sync + 'static> CALLBACK_WAKER_VTABLE<F> {
            const VTABLE: RawWakerVTable =
                RawWakerVTable::new(clone::<F>, wake::<F>, wake_by_ref::<F>, drop_waker::<F>);
        }

        let raw = RawWaker::new(
            Box::into_raw(Box::new(self)) as *const (),
            &CALLBACK_WAKER_VTABLE::<F>::VTABLE,
        );

        unsafe { Waker::from_raw(raw) }
    }
}

impl<F> Clone for CallbackWaker<F>
where
    F: Fn() + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            callback: self.callback.clone(),
        }
    }
}
