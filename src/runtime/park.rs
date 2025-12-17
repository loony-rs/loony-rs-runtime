use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;
use std::time::{Duration, Instant};

/// A thread parking primitive.
pub trait Park {
    /// Park the current thread.
    fn park(&self);

    /// Park with a timeout.
    fn park_timeout(&self, duration: Duration);

    /// Unpark the thread.
    fn unpark(&self);
}

/// Thread parking implementation using condition variables.
pub struct ThreadPark {
    inner: Arc<ThreadParkInner>,
}

struct ThreadParkInner {
    state: AtomicUsize,
    condvar: Condvar,
    mutex: Mutex<()>,
}

const EMPTY: usize = 0;
const PARKED: usize = 1;
const NOTIFIED: usize = 2;

impl ThreadPark {
    /// Create a new thread parker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ThreadParkInner {
                state: AtomicUsize::new(EMPTY),
                condvar: Condvar::new(),
                mutex: Mutex::new(()),
            }),
        }
    }

    /// Get a waker for this parker.
    pub fn waker(&self) -> ThreadWaker {
        ThreadWaker {
            inner: self.inner.clone(),
        }
    }
}

impl Park for ThreadPark {
    fn park(&self) {
        self.park_timeout(Duration::from_secs(365 * 24 * 60 * 60)) // ~1 year timeout
    }

    fn park_timeout(&self, duration: Duration) {
        let mut state = self.inner.state.load(Ordering::Acquire);

        loop {
            if state == NOTIFIED {
                match self.inner.state.compare_exchange(
                    NOTIFIED,
                    EMPTY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(s) => state = s,
                }
                continue;
            }

            match self.inner.state.compare_exchange(
                EMPTY,
                PARKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(s) => state = s,
            }
        }

        let guard = self.inner.mutex.lock().unwrap();
        let (guard, _) = self.inner.condvar.wait_timeout(guard, duration).unwrap();

        match self.inner.state.swap(EMPTY, Ordering::AcqRel) {
            NOTIFIED => {
                // Normal wakeup
            }
            PARKED => {
                // Timeout or spurious wakeup
            }
            _ => unreachable!(),
        }
    }

    fn unpark(&self) {
        let state = self.inner.state.swap(NOTIFIED, Ordering::AcqRel);

        if state == PARKED {
            let _guard = self.inner.mutex.lock().unwrap();
            self.inner.condvar.notify_one();
        }
    }
}

/// Waker for thread parker.
pub struct ThreadWaker {
    inner: Arc<ThreadParkInner>,
}

impl ThreadWaker {
    /// Convert to a standard waker.
    pub fn into_waker(self) -> Waker {
        let raw = RawWaker::new(Arc::into_raw(self.inner) as *const (), &WAKER_VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}

unsafe impl Send for ThreadWaker {}
unsafe impl Sync for ThreadWaker {}

const WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    let inner = Arc::from_raw(data as *const ThreadParkInner);
    let clone = inner.clone();
    std::mem::forget(inner);
    std::mem::forget(clone.clone());
    RawWaker::new(Arc::into_raw(clone) as *const (), &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    let inner = Arc::from_raw(data as *const ThreadParkInner);
    ThreadPark { inner }.unpark();
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    let inner = Arc::from_raw(data as *const ThreadParkInner);
    let parker = ThreadPark {
        inner: inner.clone(),
    };
    parker.unpark();
    std::mem::forget(inner);
}

unsafe fn waker_drop(data: *const ()) {
    let _ = Arc::from_raw(data as *const ThreadParkInner);
}

/// Park implementation using thread parking.
pub struct ParkThread {
    parker: ThreadPark,
}

impl ParkThread {
    /// Create a new thread parker.
    pub fn new() -> Self {
        Self {
            parker: ThreadPark::new(),
        }
    }

    /// Get the waker for this parker.
    pub fn waker(&self) -> Waker {
        self.parker.waker().into_waker()
    }

    /// Park the thread.
    pub fn park(&self) {
        self.parker.park();
    }

    /// Park with timeout.
    pub fn park_timeout(&self, duration: Duration) {
        self.parker.park_timeout(duration);
    }

    /// Unpark the thread.
    pub fn unpark(&self) {
        self.parker.unpark();
    }
}

/// A parker that uses thread sleeping for parking.
pub struct SleepPark {
    inner: Arc<SleepParkInner>,
}

struct SleepParkInner {
    state: AtomicBool,
}

impl SleepPark {
    /// Create a new sleep parker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SleepParkInner {
                state: AtomicBool::new(false),
            }),
        }
    }

    /// Get a waker for this parker.
    pub fn waker(&self) -> SleepWaker {
        SleepWaker {
            inner: self.inner.clone(),
        }
    }
}

impl Park for SleepPark {
    fn park(&self) {
        while !self.inner.state.swap(false, Ordering::AcqRel) {
            thread::park();
        }
    }

    fn park_timeout(&self, duration: Duration) {
        if !self.inner.state.swap(false, Ordering::AcqRel) {
            thread::park_timeout(duration);
        }
    }

    fn unpark(&self) {
        if !self.inner.state.swap(true, Ordering::AcqRel) {
            thread::current().unpark();
        }
    }
}

/// Waker for sleep parker.
pub struct SleepWaker {
    inner: Arc<SleepParkInner>,
}

impl SleepWaker {
    /// Convert to a standard waker.
    pub fn into_waker(self) -> Waker {
        let raw = RawWaker::new(Arc::into_raw(self.inner) as *const (), &SLEEP_WAKER_VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}

const SLEEP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    sleep_waker_clone,
    sleep_waker_wake,
    sleep_waker_wake_by_ref,
    sleep_waker_drop,
);

unsafe fn sleep_waker_clone(data: *const ()) -> RawWaker {
    let inner = Arc::from_raw(data as *const SleepParkInner);
    let clone = inner.clone();
    std::mem::forget(inner);
    std::mem::forget(clone.clone());
    RawWaker::new(Arc::into_raw(clone) as *const (), &SLEEP_WAKER_VTABLE)
}

unsafe fn sleep_waker_wake(data: *const ()) {
    let inner = Arc::from_raw(data as *const SleepParkInner);
    SleepPark { inner }.unpark();
}

unsafe fn sleep_waker_wake_by_ref(data: *const ()) {
    let inner = Arc::from_raw(data as *const SleepParkInner);
    let parker = SleepPark {
        inner: inner.clone(),
    };
    parker.unpark();
    std::mem::forget(inner);
}

unsafe fn sleep_waker_drop(data: *const ()) {
    let _ = Arc::from_raw(data as *const SleepParkInner);
}

/// A parker that yields instead of parking.
pub struct YieldPark {
    inner: Arc<YieldParkInner>,
}

struct YieldParkInner {
    should_park: AtomicBool,
}

impl YieldPark {
    /// Create a new yield parker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(YieldParkInner {
                should_park: AtomicBool::new(false),
            }),
        }
    }

    /// Get a waker for this parker.
    pub fn waker(&self) -> YieldWaker {
        YieldWaker {
            inner: self.inner.clone(),
        }
    }
}

impl Park for YieldPark {
    fn park(&self) {
        while self.inner.should_park.load(Ordering::Acquire) {
            thread::yield_now();
        }
    }

    fn park_timeout(&self, duration: Duration) {
        let start = Instant::now();
        while self.inner.should_park.load(Ordering::Acquire) {
            if start.elapsed() >= duration {
                break;
            }
            thread::yield_now();
        }
    }

    fn unpark(&self) {
        self.inner.should_park.store(false, Ordering::Release);
    }
}

/// Waker for yield parker.
pub struct YieldWaker {
    inner: Arc<YieldParkInner>,
}

impl YieldWaker {
    /// Convert to a standard waker.
    pub fn into_waker(self) -> Waker {
        let raw = RawWaker::new(Arc::into_raw(self.inner) as *const (), &YIELD_WAKER_VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}

const YIELD_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    yield_waker_clone,
    yield_waker_wake,
    yield_waker_wake_by_ref,
    yield_waker_drop,
);

unsafe fn yield_waker_clone(data: *const ()) -> RawWaker {
    let inner = Arc::from_raw(data as *const YieldParkInner);
    let clone = inner.clone();
    std::mem::forget(inner);
    std::mem::forget(clone.clone());
    RawWaker::new(Arc::into_raw(clone) as *const (), &YIELD_WAKER_VTABLE)
}

unsafe fn yield_waker_wake(data: *const ()) {
    let inner = Arc::from_raw(data as *const YieldParkInner);
    YieldPark { inner }.unpark();
}

unsafe fn yield_waker_wake_by_ref(data: *const ()) {
    let inner = Arc::from_raw(data as *const YieldParkInner);
    let parker = YieldPark {
        inner: inner.clone(),
    };
    parker.unpark();
    std::mem::forget(inner);
}

unsafe fn yield_waker_drop(data: *const ()) {
    let _ = Arc::from_raw(data as *const YieldParkInner);
}

/// Combined parker with I/O and timer support.
pub struct Parker {
    parker: Box<dyn Park + Send + Sync>,
    io_driver: Option<Arc<IoDriver>>,
    time_driver: Option<Arc<TimeDriver>>,
}

/// I/O driver for asynchronous I/O operations.
pub struct IoDriver {
    // Platform-specific I/O implementation
    selector: PlatformSelector,
    wakers: Mutex<Slab<Waker>>,
}

/// Time driver for timeouts and intervals.
pub struct TimeDriver {
    timers: Mutex<BinaryHeap<TimerEntry>>,
    waker: Condvar,
}

struct TimerEntry {
    deadline: Instant,
    waker: Waker,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.deadline.cmp(&self.deadline) // Reverse for min-heap
    }
}

impl Parker {
    /// Create a new parker with I/O support.
    pub fn new_with_io() -> io::Result<Self> {
        Ok(Self {
            parker: Box::new(ThreadPark::new()),
            io_driver: Some(Arc::new(IoDriver::new()?)),
            time_driver: Some(Arc::new(TimeDriver::new())),
        })
    }

    /// Create a new parker without I/O support.
    pub fn new() -> Self {
        Self {
            parker: Box::new(ThreadPark::new()),
            io_driver: None,
            time_driver: None,
        }
    }

    /// Park the current thread.
    pub fn park(&self) {
        self.park_timeout(None)
    }

    /// Park with an optional timeout.
    pub fn park_timeout(&self, timeout: Option<Duration>) {
        if let Some(time_driver) = &self.time_driver {
            if let Some(deadline) = time_driver.next_deadline() {
                let now = Instant::now();
                if deadline > now {
                    let duration = deadline - now;
                    if let Some(timeout) = timeout {
                        if duration < timeout {
                            self.parker.park_timeout(duration);
                            return;
                        }
                    } else {
                        self.parker.park_timeout(duration);
                        return;
                    }
                }
            }
        }

        if let Some(timeout) = timeout {
            self.parker.park_timeout(timeout);
        } else {
            self.parker.park();
        }
    }

    /// Unpark the thread.
    pub fn unpark(&self) {
        self.parker.unpark();
    }

    /// Get the I/O driver.
    pub fn io_driver(&self) -> Option<&IoDriver> {
        self.io_driver.as_deref()
    }

    /// Get the time driver.
    pub fn time_driver(&self) -> Option<&TimeDriver> {
        self.time_driver.as_deref()
    }
}

impl IoDriver {
    fn new() -> io::Result<Self> {
        Ok(Self {
            selector: PlatformSelector::new()?,
            wakers: Mutex::new(Slab::new()),
        })
    }

    /// Register an I/O source.
    pub fn register(
        &self,
        source: &mut dyn Source,
        token: usize,
        interests: Interest,
    ) -> io::Result<()> {
        self.selector.register(source, token, interests)
    }

    /// Reregister an I/O source.
    pub fn reregister(
        &self,
        source: &mut dyn Source,
        token: usize,
        interests: Interest,
    ) -> io::Result<()> {
        self.selector.reregister(source, token, interests)
    }

    /// Deregister an I/O source.
    pub fn deregister(&self, source: &mut dyn Source) -> io::Result<()> {
        self.selector.deregister(source)
    }

    /// Poll for I/O events.
    pub fn poll(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut events = Events::with_capacity(1024);
        self.selector.select(&mut events, timeout)?;

        for event in &events {
            let token = event.token();
            if let Some(waker) = self.wakers.lock().unwrap().get(token) {
                waker.wake_by_ref();
            }
        }

        Ok(())
    }
}

impl TimeDriver {
    fn new() -> Self {
        Self {
            timers: Mutex::new(BinaryHeap::new()),
            waker: Condvar::new(),
        }
    }

    /// Schedule a timer.
    pub fn schedule(&self, deadline: Instant, waker: Waker) {
        let mut timers = self.timers.lock().unwrap();
        timers.push(TimerEntry { deadline, waker });
        self.waker.notify_one();
    }

    /// Get the next deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.lock().unwrap().peek().map(|e| e.deadline)
    }

    /// Process expired timers.
    pub fn process(&self) {
        let now = Instant::now();
        let mut timers = self.timers.lock().unwrap();

        while let Some(entry) = timers.peek() {
            if entry.deadline > now {
                break;
            }

            let entry = timers.pop().unwrap();
            entry.waker.wake();
        }
    }
}

/// Interest in I/O operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    Readable,
    Writable,
    Both,
}

/// Trait for I/O sources.
pub trait Source {
    fn try_clone(&self) -> io::Result<Box<dyn Source>>;
}

/// Platform-specific selector.
struct PlatformSelector {
    #[cfg(unix)]
    inner: mio::Poll,
    #[cfg(windows)]
    inner: iocp::CompletionPort,
}

impl PlatformSelector {
    fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: mio::Poll::new()?,
            })
        }

        #[cfg(windows)]
        {
            Ok(Self {
                inner: iocp::CompletionPort::new(0)?,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "platform not supported",
            ))
        }
    }

    fn register(
        &self,
        source: &mut dyn Source,
        token: usize,
        interests: Interest,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            use mio::{Interest as MioInterest, Token};

            let mio_interests = match interests {
                Interest::Readable => MioInterest::READABLE,
                Interest::Writable => MioInterest::WRITABLE,
                Interest::Both => MioInterest::READABLE | MioInterest::WRITABLE,
            };

            self.inner.register(source, Token(token), mio_interests)
        }

        #[cfg(windows)]
        {
            // Windows implementation
            Ok(())
        }
    }

    fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.poll(events, timeout)
        }

        #[cfg(windows)]
        {
            // Windows implementation
            Ok(())
        }
    }

    fn deregister(&self, source: &mut dyn Source) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.deregister(source)
        }

        #[cfg(windows)]
        {
            // Windows implementation
            Ok(())
        }
    }

    fn reregister(
        &self,
        source: &mut dyn Source,
        token: usize,
        interests: Interest,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            use mio::{Interest as MioInterest, Token};

            let mio_interests = match interests {
                Interest::Readable => MioInterest::READABLE,
                Interest::Writable => MioInterest::WRITABLE,
                Interest::Both => MioInterest::READABLE | MioInterest::WRITABLE,
            };

            self.inner.reregister(source, Token(token), mio_interests)
        }

        #[cfg(windows)]
        {
            // Windows implementation
            Ok(())
        }
    }
}

/// I/O events.
pub struct Events {
    #[cfg(unix)]
    inner: mio::Events,
    #[cfg(windows)]
    inner: Vec<iocp::Event>,
}

impl Events {
    fn with_capacity(capacity: usize) -> Self {
        #[cfg(unix)]
        {
            Self {
                inner: mio::Events::with_capacity(capacity),
            }
        }

        #[cfg(windows)]
        {
            Self {
                inner: Vec::with_capacity(capacity),
            }
        }
    }
}

impl<'a> IntoIterator for &'a Events {
    type Item = Event;
    type IntoIter = EventsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        EventsIter {
            events: self,
            index: 0,
        }
    }
}

pub struct EventsIter<'a> {
    events: &'a Events,
    index: usize,
}

impl<'a> Iterator for EventsIter<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        #[cfg(unix)]
        {
            if let Some(event) = self.events.inner.get(self.index) {
                self.index += 1;
                Some(Event {
                    token: event.token().0,
                })
            } else {
                None
            }
        }

        #[cfg(windows)]
        {
            if self.index < self.events.inner.len() {
                let event = &self.events.inner[self.index];
                self.index += 1;
                Some(Event {
                    token: event.token(),
                })
            } else {
                None
            }
        }
    }
}

/// I/O event.
pub struct Event {
    token: usize,
}

impl Event {
    /// Get the event token.
    pub fn token(&self) -> usize {
        self.token
    }
}

use slab::Slab;
use std::collections::BinaryHeap;
use std::io;
