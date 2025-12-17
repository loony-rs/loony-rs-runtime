// src/time/driver.rs
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct TimeEntry {
    deadline: Instant,
    waker: std::task::Waker,
}

pub struct TimeDriver {
    inner: Arc<Mutex<TimeDriverInner>>,
}

struct TimeDriverInner {
    timers: BinaryHeap<Reverse<TimeEntry>>,
    waker: Option<std::task::Waker>,
}

impl TimeDriver {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TimeDriverInner {
                timers: BinaryHeap::new(),
                waker: None,
            })),
        }
    }

    pub fn sleep(&self, duration: Duration) -> Sleep {
        let deadline = Instant::now() + duration;
        let inner = self.inner.clone();

        Sleep {
            inner,
            deadline,
            registered: false,
        }
    }
}

pub struct Sleep {
    inner: Arc<Mutex<TimeDriverInner>>,
    deadline: Instant,
    registered: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        if Instant::now() >= self.deadline {
            return Poll::Ready(());
        }

        if !self.registered {
            let mut inner = self.inner.lock().unwrap();

            inner.timers.push(Reverse(TimeEntry {
                deadline: self.deadline,
                waker: cx.waker().clone(),
            }));

            // Wake up the time driver thread
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }

            self.registered = true;
        }

        Poll::Pending
    }
}
