//! wait

use parking_lot::Mutex;
use std::{
    ffi::{c_void, OsString},
    future::Future,
    io,
    os::windows::{
        io::{AsRawHandle, HandleOrNull, OwnedHandle, RawHandle},
        prelude::*,
    },
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{FALSE, FILETIME, TRUE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CloseThreadpoolWait, CreateEventW, CreateThreadpoolWait, ResetEvent, SetEvent,
        SetThreadpoolWait, WaitForSingleObject, WaitForThreadpoolWaitCallbacks, INFINITE,
        PTP_CALLBACK_INSTANCE, PTP_WAIT,
    },
};

/// Waiting on a waitable object will resolve with Ok or a WaitError
pub type WaitResult = Result<(), WaitError>;

/// When waiting on a waitable object. The wait may resolve with a wait error.
#[derive(thiserror::Error, Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum WaitError {
    /// A caller signaled they are no longer interested in waiting for the wait object.
    #[error("wait cancelled")]
    Cancelled,
    /// The waitable object failed to complete before the specified timeout
    #[error("wait timeout")]
    Timeout = WAIT_TIMEOUT,
    /// Already waiting for the waitable object
    #[error("wait already in progress")]
    InProgress,
}

/// Waitable object as per windows
pub trait Waitable: AsRawHandle {}

/// Wait for pending threadpool callbacks, or cancel pending threadpool callbacks
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WaitPending {
    /// Wait for pending threadpool callbacks
    Wait = 0,
    /// Cancel pending threadpool callbacks
    Cancel = 1,
}

/// Callback for which to register Wait handlers
type WaitCallback = unsafe extern "system" fn(PTP_CALLBACK_INSTANCE, *mut c_void, PTP_WAIT, u32);

#[derive(Debug)]
pub struct WaitPool(PTP_WAIT);
impl Drop for WaitPool {
    fn drop(&mut self) {
        self.stop();
        self.wait(WaitPending::Cancel);
        unsafe { CloseThreadpoolWait(self.0) }
    }
}

impl WaitPool {
    /// https://learn.microsoft.com/en-us/windows/win32/api/threadpoolapiset/nf-threadpoolapiset-createthreadpoolwait
    pub fn new(cx: *mut c_void, callback: WaitCallback) -> io::Result<Self> {
        let result = unsafe { CreateThreadpoolWait(Some(callback), cx, std::ptr::null()) };
        match result {
            0 => Err(io::Error::last_os_error()),
            handle => Ok(WaitPool(handle)),
        }
    }

    /// Set a wait object on a threadpool which will trigger a wait callback
    ///
    /// https://learn.microsoft.com/en-us/windows/win32/api/threadpoolapiset/nf-threadpoolapiset-setthreadpoolwait
    pub fn start<W: Waitable>(&self, waitable: &W, timeout: Option<Duration>) {
        let ft = timeout
            .map(|to| {
                let ms = to.as_millis();
                &FILETIME {
                    dwHighDateTime: (ms >> 32) as u32,
                    dwLowDateTime: (ms & 0xFFFFFFFF) as u32,
                } as *const _
            })
            .unwrap_or_else(std::ptr::null);
        unsafe { SetThreadpoolWait(self.0, waitable.as_raw_handle() as _, ft) };
    }

    /// The wait object will cease to queue new callbacks. Callbacks already queued will still fire
    ///
    /// https://learn.microsoft.com/en-us/windows/win32/api/threadpoolapiset/nf-threadpoolapiset-setthreadpoolwait
    pub fn stop(&self) {
        let ft = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // Handle == 0 pool will cease to queue new callbacks. existing callbacks still occur
        unsafe { SetThreadpoolWait(self.0, 0, &ft) };
    }

    /// Waits for outstanding wait callbacks to complete and optionally cancels pending callbacks
    /// that have not yet started to execute.
    ///
    /// https://learn.microsoft.com/en-us/windows/win32/api/threadpoolapiset/nf-threadpoolapiset-waitforthreadpoolwaitcallbacks
    pub fn wait(&self, pending: WaitPending) {
        unsafe { WaitForThreadpoolWaitCallbacks(self.0, pending as _) };
    }
}

#[derive(Default, Debug)]
struct WaitState {
    waker: Option<Waker>,
    result: Option<WaitResult>,
}

#[derive(Debug, Clone)]
pub struct Waiting(Arc<Mutex<WaitState>>);

impl Future for Waiting {
    type Output = WaitResult;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared = self.0.lock();
        let new_waker = cx.waker();

        match shared.result {
            Some(result) => {
                // If a result is ready, wake executor with result
                if let Some(waker) = shared.waker.take() {
                    waker.wake()
                }
                Poll::Ready(result)
            }
            None => {
                // Update our waker
                shared.waker = match shared.waker.take() {
                    None => Some(new_waker.clone()),
                    Some(old_waker) => match old_waker.will_wake(new_waker) {
                        false => Some(new_waker.clone()),
                        true => Some(old_waker),
                    },
                };
                Poll::Pending
            }
        }
    }
}

unsafe extern "system" fn wait_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _wait: PTP_WAIT,
    waitresult: u32,
) {
    let state = &*(context as *const Mutex<WaitState>);
    let mut shared = state.lock();
    shared.result = match waitresult {
        WAIT_OBJECT_0 => Some(Ok(())),
        WAIT_TIMEOUT => Some(Err(WaitError::Timeout)),
        _ => panic!("Unsupported kernel argument passed to wait callback!"),
    };
    if let Some(waker) = shared.waker.as_ref() {
        waker.wake_by_ref()
    }
}

/// Windows CreateEvent creation argument
///
/// If this parameter is TRUE, the function creates a manual-reset event object,
/// which requires the use of the ResetEvent function to set the event state to
/// nonsignaled. If this parameter is FALSE, the function creates an auto-reset
/// event object, and the system automatically resets the event state to
/// nonsignaled after a single waiting thread has been released.
#[repr(i32)]
#[derive(PartialEq)]
pub enum EventReset {
    Manual = TRUE,
    Automatic = FALSE,
}

/// Windows CreateEvent creation argument
///
/// If this parameter is TRUE, the initial state of the event object is
/// signaled; otherwise, it is nonsignaled.
#[repr(i32)]
pub enum EventInitialState {
    Set = TRUE,
    Unset = FALSE,
}

#[derive(Debug)]
pub struct Event(OwnedHandle);

impl Event {
    /// Create a system event
    ///
    /// [CreateEventW](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createeventa)
    pub fn named<O>(name: O, reset: EventReset, state: EventInitialState) -> io::Result<Event>
    where
        O: Into<OsString>,
    {
        let kernel_name = name
            .into()
            .encode_wide()
            .chain(Some(0).into_iter())
            .collect::<Vec<_>>();
        Self::new_raw(kernel_name.as_ptr() as _, reset, state)
    }

    /// Create a system event with out a name
    ///
    /// [CreateEventW](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createeventa)
    pub fn anonymous(reset: EventReset, state: EventInitialState) -> io::Result<Event> {
        Self::new_raw(std::ptr::null(), reset, state)
    }

    pub fn new_raw(
        name: *const u16,
        reset: EventReset,
        state: EventInitialState,
    ) -> io::Result<Event> {
        unsafe {
            let raw = CreateEventW(std::ptr::null(), reset as _, state as _, name);
            let handle = HandleOrNull::from_raw_handle(raw as _);
            OwnedHandle::try_from(handle).map_err(|_| io::Error::last_os_error())
        }
        .map(Self)
    }

    pub fn set(&self) -> io::Result<()> {
        match unsafe { SetEvent(self.as_raw_handle() as _) } {
            FALSE => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    pub fn reset(&self) -> io::Result<()> {
        match unsafe { ResetEvent(self.as_raw_handle() as _) } {
            FALSE => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    pub fn wait(&self, duration: Option<Duration>) -> Result<(), EventError> {
        let dur: u32 = duration.map(|d| d.as_millis() as _).unwrap_or(INFINITE);
        match unsafe { WaitForSingleObject(self.as_raw_handle() as _, dur as _) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_ABANDONED => Err(EventError::Abandoned),
            WAIT_FAILED => Err(EventError::Failed),
            WAIT_TIMEOUT => Err(EventError::Timeout),
            _ => Err(EventError::Io(io::Error::last_os_error())),
        }
    }
}

impl Waitable for Event {}

impl AsRawHandle for Event {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}

#[derive(thiserror::Error, Debug)]
#[repr(u32)]
pub enum EventError {
    #[error("event abandoned")]
    Abandoned = WAIT_ABANDONED,
    #[error("wait failed")]
    Failed = WAIT_FAILED,
    #[error("wait timeout")]
    Timeout = WAIT_TIMEOUT,
    #[error("io error => {0}")]
    Io(#[from] io::Error),
}

/// A handle to pool of workers who wait for wait objects . The context is also shared by the
/// futures and weakly by the kernel. The weak reference used by the kernel is guarenteed to be
/// valid because the threadpool will wait for all kernel callbacks to resolve prior to
/// dropping.
///
/// This is guarenteed because RFC 1857 specifying drop order. The threadpool is dropped first
/// and waits for callbacks to finish executing. Then the pointers are dropped.
///
/// Safety: DO NOT CHANGE ORDER IN STRUCT (RFC 1857)
#[derive(Debug)]
pub struct EventListener {
    /// A pool of workers to wait on waitable objects. See [`self::WaitPool`]. NOTE the
    pool: WaitPool,
    /// Shared state between the waitable worker callbacks and future waiting for event
    state: Arc<Mutex<WaitState>>,
}

impl EventListener {
    pub fn new() -> io::Result<Self> {
        let state = Arc::new(Mutex::new(WaitState::default()));
        WaitPool::new(Arc::as_ptr(&state) as _, wait_callback).map(|pool| Self { pool, state })
    }

    pub fn start<W>(&self, waitable: &W, timeout: Option<Duration>) -> Waiting
    where
        W: Waitable,
    {
        let state = self.state.lock();
        if let None = state.result {
            self.pool.start(waitable, timeout);
            Waiting(Arc::clone(&self.state))
        } else {
            panic!("Cannot start waiting more than once! use restart instead")
        }
    }

    pub fn restart<W>(&self, waitable: &W, timeout: Option<Duration>) -> Result<Waiting, WaitError>
    where
        W: Waitable,
    {
        let mut state = self.state.lock();
        let _result = state.result.take().ok_or(WaitError::InProgress)?;
        self.pool.start(waitable, timeout);
        Ok(Waiting(Arc::clone(&self.state)))
    }

    pub fn cancel(&self) -> &Self {
        self.pool.stop();
        let mut state = self.state.lock();
        match state.result.replace(Err(WaitError::Cancelled)) {
            Some(prev) => state.result = Some(prev),
            None => match state.waker.take() {
                Some(waker) => waker.wake(),
                None => {}
            },
        }
        self
    }
}

#[derive(Debug)]
pub struct Receiver {
    #[allow(unused)]
    pool: WaitPool,
    state: Arc<(Mutex<WaitState>, Event)>,
}

impl Future for Receiver {
    type Output = WaitResult;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.0.lock();
        let new_waker = cx.waker();

        match state.result {
            Some(result) => {
                if let Some(waker) = state.waker.take() {
                    waker.wake()
                }
                Poll::Ready(result)
            }
            None => {
                // Update our waker
                state.waker = match state.waker.take() {
                    None => Some(new_waker.clone()),
                    Some(old_waker) => match old_waker.will_wake(new_waker) {
                        false => Some(new_waker.clone()),
                        true => Some(old_waker),
                    },
                };
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
pub struct Sender {
    #[allow(unused)]
    state: Arc<(Mutex<WaitState>, Event)>,
}

impl Sender {
    pub fn set(self) -> io::Result<()> {
        self.state.1.set()
    }
}

pub fn oneshot() -> io::Result<(Sender, Receiver)> {
    let event = Event::anonymous(EventReset::Manual, EventInitialState::Unset)?;
    let state = Arc::new((Mutex::new(WaitState::default()), event));
    let pool = WaitPool::new(Arc::as_ptr(&state) as _, oneshot_callback)?;
    pool.start(&state.1, None);
    let sender = Sender { state };
    let receiver = Receiver {
        state: Arc::clone(&sender.state),
        pool,
    };
    Ok((sender, receiver))
}

unsafe extern "system" fn oneshot_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _wait: PTP_WAIT,
    waitresult: u32,
) {
    let state = &*(context as *const (Mutex<WaitState>, Event));
    let mut shared = state.0.lock();
    shared.result = match waitresult {
        WAIT_OBJECT_0 => Some(Ok(())),
        WAIT_TIMEOUT => Some(Err(WaitError::Timeout)),
        _ => panic!("Unsupported kernel argument passed to wait callback!"),
    };
    if let Some(waker) = shared.waker.as_ref() {
        waker.wake_by_ref()
    }
}
