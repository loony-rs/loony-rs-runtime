mod raw;
mod waker;

use raw::{Header, Vtable};
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
use std::ptr::NonNull;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

pub use raw::Task;

pub struct JoinHandle<T> {
    raw: NonNull<Header>,
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        // Set abort flag
        unsafe {
            let state = (*self.raw.as_ptr())
                .state
                .fetch_or(0x4, std::sync::atomic::Ordering::AcqRel);
            if state & 0x2 != 0 {
                // Task is scheduled, we need to remove it
            }
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let header = self.raw.as_ptr();
            let state = (*header).state.load(std::sync::atomic::Ordering::Acquire);

            if state & 0x1 != 0 {
                // Task is complete
                let output = Box::from_raw((*header).output.get() as *mut Option<T>);
                Poll::Ready(Ok(output.take().unwrap()))
            } else if state & 0x4 != 0 {
                // Task was aborted
                Poll::Ready(Err(JoinError::Aborted))
            } else {
                // Store waker
                (*header).waker.get().replace(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
pub enum JoinError {
    Aborted,
    Panic,
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    create_task(future, Arc::new(())).1
}

pub(crate) fn create_task<F, S>(future: F, _scheduler: Arc<S>) -> (Task, JoinHandle<F::Output>)
where
    F: Future + 'static,
    F::Output: 'static,
{
    let vtable = &raw::VTABLE::<F>;

    let header = NonNull::from(Box::leak(Box::new(Header {
        state: std::sync::atomic::AtomicUsize::new(0),
        scheduler: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        vtable: &vtable.0,
        future: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(future)) as *mut ()),
        output: std::cell::UnsafeCell::new(Box::into_raw(Box::new(None::<F::Output>)) as *mut ()),
        waker: std::cell::UnsafeCell::new(None),
    })));

    (
        Task { header },
        JoinHandle {
            raw: header,
            _marker: PhantomData,
        },
    )
}

unsafe fn poll_task<F: Future>(header: NonNull<Header>) {
    let future = &mut *((*header.as_ptr())
        .future
        .load(std::sync::atomic::Ordering::Acquire) as *mut F);
    let waker = raw::waker_from_task(header);
    let mut cx = Context::from_waker(&waker);

    match std::pin::Pin::new_unchecked(future).poll(&mut cx) {
        Poll::Ready(output) => {
            // Store output
            let output_ptr = (*header.as_ptr()).output.get() as *mut Option<F::Output>;
            *output_ptr = Some(output);

            // Mark as complete
            (*header.as_ptr())
                .state
                .fetch_or(0x1, std::sync::atomic::Ordering::Release);

            // Wake join handle
            if let Some(waker) = (*header.as_ptr()).waker.get().replace(None) {
                waker.wake();
            }
        }
        Poll::Pending => {}
    }
}

unsafe fn deallocate_task<F: Future>(header: NonNull<Header>) {
    let future_ptr = (*header.as_ptr())
        .future
        .load(std::sync::atomic::Ordering::Acquire) as *mut F;
    let _ = Box::from_raw(future_ptr);

    let output_ptr = (*header.as_ptr()).output.get() as *mut Option<F::Output>;
    let _ = Box::from_raw(output_ptr);

    let _ = Box::from_raw(header.as_ptr());
}
