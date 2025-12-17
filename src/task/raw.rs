use std::any::Any;
use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// Task header containing metadata and pointers.
///
/// Layout:
/// [Header][Future][Output][JoinWaker]
pub struct Header {
    /// Task state bits:
    /// - Bits 0-1: State (IDLE=0, SCHEDULED=1, RUNNING=2, COMPLETE=3)
    /// - Bits 2-15: Flags (ABORTED=0x4, JOIN_INTEREST=0x8, etc.)
    /// - Bits 16-31: Waker reference count
    pub(crate) state: AtomicUsize,

    /// Pointer to scheduler data for waking
    pub(crate) scheduler: AtomicPtr<SchedulerData>,

    /// Vtable for type-erased operations
    pub(crate) vtable: &'static Vtable,

    /// Pointer to the future (type-erased)
    pub(crate) future: AtomicPtr<()>,

    /// Pointer to the output storage (type-erased)
    pub(crate) output: UnsafeCell<*mut ()>,

    /// Waker for the join handle
    pub(crate) join_waker: UnsafeCell<Option<Waker>>,
}

/// Scheduler data passed to tasks
#[derive(Debug)]
pub struct SchedulerData {
    /// Function to schedule a task
    pub schedule: unsafe fn(*const Header),
    /// User data for the scheduler
    pub data: *const (),
}

/// Vtable for type-erased task operations
pub struct Vtable {
    /// Poll the future
    pub poll: unsafe fn(NonNull<Header>),
    /// Deallocate the task
    pub deallocate: unsafe fn(NonNull<Header>),
    /// Destroy the future (without deallocating)
    pub destroy_future: unsafe fn(*mut ()),
    /// Destroy the output (without deallocating)
    pub destroy_output: unsafe fn(*mut ()),
}

/// A raw task that can be scheduled and run
pub struct Task {
    pub(crate) header: NonNull<Header>,
}

impl Task {
    /// Create a new task from a header pointer
    pub(crate) unsafe fn from_raw(ptr: NonNull<Header>) -> Self {
        Self { header: ptr }
    }

    /// Get the raw pointer to the header
    pub(crate) fn into_raw(self) -> NonNull<Header> {
        self.header
    }

    /// Run the task
    pub(crate) fn run(self) {
        unsafe {
            let vtable = (*self.header.as_ptr()).vtable;
            (vtable.poll)(self.header);
        }
    }

    /// Schedule the task using its scheduler
    pub(crate) fn schedule(self) {
        unsafe {
            let header_ptr = self.header.as_ptr();
            let scheduler = (*header_ptr).scheduler.load(Ordering::Acquire);

            if !scheduler.is_null() {
                let scheduler_data = &*scheduler;
                (scheduler_data.schedule)(header_ptr);
            } else {
                // Fallback: run immediately on current thread
                self.run();
            }
        }
    }

    /// Get the task state
    pub(crate) fn state(&self) -> TaskState {
        let state = unsafe { (*self.header.as_ptr()).state.load(Ordering::Acquire) };
        TaskState::from_bits(state)
    }

    /// Set the task state
    pub(crate) fn set_state(&self, state: TaskState) {
        unsafe {
            (*self.header.as_ptr())
                .state
                .store(state.bits(), Ordering::Release);
        }
    }

    /// Transition state atomically
    pub(crate) fn transition_state(&self, from: TaskState, to: TaskState) -> bool {
        unsafe {
            (*self.header.as_ptr())
                .state
                .compare_exchange(from.bits(), to.bits(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        }
    }
}

// Safety: Task is just a pointer, can be sent between threads
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

/// Task state bits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskState {
    bits: usize,
}

impl TaskState {
    pub const STATE_MASK: usize = 0x3;
    pub const FLAGS_MASK: usize = 0xFFFC;
    pub const REFCOUNT_SHIFT: usize = 16;
    pub const REFCOUNT_MASK: usize = 0xFFFF0000;

    pub const IDLE: Self = Self { bits: 0 };
    pub const SCHEDULED: Self = Self { bits: 1 };
    pub const RUNNING: Self = Self { bits: 2 };
    pub const COMPLETE: Self = Self { bits: 3 };

    pub const ABORTED: Self = Self { bits: 0x4 };
    pub const JOIN_INTEREST: Self = Self { bits: 0x8 };

    pub fn from_bits(bits: usize) -> Self {
        Self { bits }
    }

    pub fn bits(self) -> usize {
        self.bits
    }

    pub fn state(self) -> usize {
        self.bits & Self::STATE_MASK
    }

    pub fn flags(self) -> usize {
        self.bits & Self::FLAGS_MASK
    }

    pub fn refcount(self) -> usize {
        (self.bits & Self::REFCOUNT_MASK) >> Self::REFCOUNT_SHIFT
    }

    pub fn is_idle(self) -> bool {
        self.state() == Self::IDLE.bits()
    }

    pub fn is_scheduled(self) -> bool {
        self.state() == Self::SCHEDULED.bits()
    }

    pub fn is_running(self) -> bool {
        self.state() == Self::RUNNING.bits()
    }

    pub fn is_complete(self) -> bool {
        self.state() == Self::COMPLETE.bits()
    }

    pub fn is_aborted(self) -> bool {
        (self.bits & Self::ABORTED.bits()) != 0
    }

    pub fn has_join_interest(self) -> bool {
        (self.bits & Self::JOIN_INTEREST.bits()) != 0
    }

    pub fn with_refcount(self, refcount: usize) -> Self {
        let bits = (self.bits & !Self::REFCOUNT_MASK)
            | ((refcount << Self::REFCOUNT_SHIFT) & Self::REFCOUNT_MASK);
        Self { bits }
    }

    pub fn with_flag(self, flag: Self) -> Self {
        Self {
            bits: self.bits | flag.bits,
        }
    }

    pub fn without_flag(self, flag: Self) -> Self {
        Self {
            bits: self.bits & !flag.bits,
        }
    }
}

/// Type-specific vtable for a future
pub struct VTableFor<F, T>
where
    F: Future<Output = T>,
{
    vtable: Vtable,
    _marker: PhantomData<(F, T)>,
}

impl<F, T> VTableFor<F, T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    pub const fn new() -> Self {
        Self {
            vtable: Vtable {
                poll: Self::poll,
                deallocate: Self::deallocate,
                destroy_future: Self::destroy_future,
                destroy_output: Self::destroy_output,
            },
            _marker: PhantomData,
        }
    }

    unsafe fn poll(header: NonNull<Header>) {
        // Get the future pointer
        let future_ptr = (*header.as_ptr()).future.load(Ordering::Acquire) as *mut F;

        // Create waker for the task
        let waker = waker_from_task(header);
        let mut cx = Context::from_waker(&waker);

        // Poll the future
        let future = Pin::new_unchecked(&mut *future_ptr);
        match future.poll(&mut cx) {
            Poll::Ready(output) => {
                // Store the output
                let output_ptr = (*header.as_ptr()).output.get();
                ptr::write(output_ptr as *mut T, output);

                // Mark task as complete
                let prev_state = (*header.as_ptr())
                    .state
                    .fetch_or(TaskState::COMPLETE.bits(), Ordering::AcqRel);

                // Wake join handle if interested
                if (prev_state & TaskState::JOIN_INTEREST.bits()) != 0 {
                    if let Some(join_waker) = (*header.as_ptr()).join_waker.get().take() {
                        join_waker.wake();
                    }
                }

                // Decrement the refcount (we were holding one for polling)
                let new_state = (*header.as_ptr())
                    .state
                    .fetch_sub(1 << TaskState::REFCOUNT_SHIFT, Ordering::AcqRel);

                // If last waker and task is complete, schedule deallocation
                if TaskState::from_bits(new_state).refcount() == 1 {
                    // Task will be deallocated when last waker is dropped
                }
            }
            Poll::Pending => {
                // Future is still pending
                // Decrement the refcount (we were holding one for polling)
                let _ = (*header.as_ptr())
                    .state
                    .fetch_sub(1 << TaskState::REFCOUNT_SHIFT, Ordering::AcqRel);
            }
        }
    }

    unsafe fn deallocate(header: NonNull<Header>) {
        let vtable = (*header.as_ptr()).vtable;

        // Destroy the future
        let future_ptr = (*header.as_ptr()).future.load(Ordering::Acquire);
        if !future_ptr.is_null() {
            (vtable.destroy_future)(future_ptr);
        }

        // Destroy the output if present
        let output_ptr = (*header.as_ptr()).output.get();
        if !output_ptr.is_null()
            && (*header.as_ptr()).state.load(Ordering::Acquire) & TaskState::COMPLETE.bits() != 0
        {
            (vtable.destroy_output)(output_ptr);
        }

        // Deallocate the header and all memory
        let layout = Self::task_layout::<F, T>();
        let ptr = header.as_ptr() as *mut u8;
        std::alloc::dealloc(ptr, layout);
    }

    unsafe fn destroy_future(ptr: *mut ()) {
        let _ = Box::from_raw(ptr as *mut F);
    }

    unsafe fn destroy_output(ptr: *mut ()) {
        let _ = Box::from_raw(ptr as *mut T);
    }

    fn task_layout<F2, T2>() -> std::alloc::Layout
    where
        F2: Future<Output = T2>,
    {
        let header_size = mem::size_of::<Header>();
        let future_size = mem::size_of::<F2>();
        let output_size = mem::size_of::<T2>();

        let align = mem::align_of::<Header>()
            .max(mem::align_of::<F2>())
            .max(mem::align_of::<T2>());

        let total_size = header_size + future_size + output_size;

        std::alloc::Layout::from_size_align(total_size, align).expect("Invalid task layout")
    }
}

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
        wake_by_ref_waker(data);
    }

    unsafe fn wake_by_ref_waker(data: *const ()) {
        let header = NonNull::new_unchecked(data as *mut Header);

        // Try to transition from IDLE to SCHEDULED
        loop {
            let current = (*header.as_ptr()).state.load(Ordering::Acquire);
            let state = TaskState::from_bits(current);

            if state.is_complete() || state.is_aborted() {
                // Task is already done
                break;
            }

            if state.is_scheduled() || state.is_running() {
                // Already scheduled or running
                break;
            }

            // Try to transition from IDLE to SCHEDULED
            let new_state = state.with_flag(TaskState::SCHEDULED).bits();
            match (*header.as_ptr()).state.compare_exchange(
                current,
                new_state,
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

/// Create a raw task from a future
pub(crate) fn create_raw_task<F, T, S>(future: F, scheduler_data: S) -> (Task, NonNull<Header>)
where
    F: Future<Output = T> + 'static,
    T: 'static,
    S: Fn(*const Header),
{
    // Create vtable
    static VTABLE: VTableFor<F, T> = VTableFor::<F, T>::new();

    // Calculate layout
    let layout = VTableFor::<F, T>::task_layout::<F, T>();

    // Allocate memory for the entire task
    let ptr = unsafe { std::alloc::alloc(layout) } as *mut Header;

    if ptr.is_null() {
        panic!("Failed to allocate task memory");
    }

    let header = NonNull::new(ptr).unwrap();

    // Initialize the future in-place (after the header)
    let future_ptr = unsafe { ptr.add(1) as *mut F };
    unsafe {
        ptr::write(future_ptr, future);
    }

    // Initialize output storage (after the future)
    let output_ptr = unsafe { future_ptr.add(1) as *mut T as *mut () };

    // Initialize the header
    unsafe {
        ptr::write(
            ptr,
            Header {
                state: AtomicUsize::new(TaskState::IDLE.with_refcount(1).bits()), // 1 for initial waker
                scheduler: AtomicPtr::new(Box::into_raw(Box::new(SchedulerData {
                    schedule: schedule_fn::<S>,
                    data: Box::into_raw(Box::new(scheduler_data)) as *const (),
                }))),
                vtable: &VTABLE.vtable,
                future: AtomicPtr::new(future_ptr as *mut ()),
                output: UnsafeCell::new(output_ptr),
                join_waker: UnsafeCell::new(None),
            },
        );
    }

    (Task { header }, header)
}

/// Schedule function that calls the user-provided scheduler
unsafe fn schedule_fn<S>(header_ptr: *const Header)
where
    S: Fn(*const Header),
{
    let header = &*header_ptr;
    let scheduler_ptr = header.scheduler.load(Ordering::Acquire);

    if !scheduler_ptr.is_null() {
        let scheduler_data = &*scheduler_ptr;
        let user_data = scheduler_data.data as *const S;
        let scheduler = &*user_data;
        scheduler(header_ptr);
    }
}

/// Set the join waker for a task
pub(crate) unsafe fn set_join_waker(header: NonNull<Header>, waker: Waker) -> bool {
    let state_ptr = header.as_ptr();

    // Set JOIN_INTEREST flag
    let prev_state = (*state_ptr)
        .state
        .fetch_or(TaskState::JOIN_INTEREST.bits(), Ordering::AcqRel);

    // Store the waker
    (*state_ptr).join_waker.get().replace(waker);

    // Return true if task is already complete
    (prev_state & TaskState::COMPLETE.bits()) != 0
}

/// Try to read the task output
pub(crate) unsafe fn try_read_output<T>(header: NonNull<Header>) -> Option<T> {
    let state = (*header.as_ptr()).state.load(Ordering::Acquire);

    if (state & TaskState::COMPLETE.bits()) == 0 {
        return None;
    }

    if (state & TaskState::ABORTED.bits()) != 0 {
        // Task was aborted
        return None;
    }

    // Read the output
    let output_ptr = (*header.as_ptr()).output.get() as *const T;
    Some(ptr::read(output_ptr))
}

/// Abort a task
pub(crate) unsafe fn abort_task(header: NonNull<Header>) {
    let state_ptr = header.as_ptr();

    // Set ABORTED flag
    (*state_ptr)
        .state
        .fetch_or(TaskState::ABORTED.bits(), Ordering::AcqRel);

    // Wake join handle if interested
    let state = (*state_ptr).state.load(Ordering::Acquire);
    if (state & TaskState::JOIN_INTEREST.bits()) != 0 {
        if let Some(join_waker) = (*state_ptr).join_waker.get().take() {
            join_waker.wake();
        }
    }
}
