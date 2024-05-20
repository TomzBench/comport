//! channel

use bytes::{Buf, BufMut, BytesMut};
use crossbeam::queue::ArrayQueue;
use futures::{AsyncRead, AsyncWrite, Stream};
use parking_lot::Mutex;
use pin_project_lite::pin_project;
use std::{
    io,
    os::windows::io::{AsRawHandle, RawHandle},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use windows_sys::Win32::{Foundation::FALSE, System::IO::CancelIoEx};

pub trait WakeHandle: AsRawHandle {
    fn wake(&self) -> io::Result<()> {
        let result = unsafe { CancelIoEx(self.as_raw_handle() as _, std::ptr::null()) };
        match result {
            FALSE => Err(io::Error::last_os_error().into()),
            _ => Ok(()),
        }
    }
}

pub struct RawWakeHandle(RawHandle);

impl RawWakeHandle {
    pub fn from_raw_handle<H: AsRawHandle>(handle: &H) -> RawWakeHandle {
        RawWakeHandle(handle.as_raw_handle())
    }
}

impl AsRawHandle for RawWakeHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.0
    }
}

impl WakeHandle for RawWakeHandle {}

pub fn bounded<W>(handle: W, capacity: usize) -> (TaskQueue<W>, ThreadQueue)
where
    W: WakeHandle,
{
    let state = Arc::new(State {
        task: ArrayQueue::new(capacity),
        thread: ArrayQueue::new(capacity),
        read_waker: Mutex::new(None),
        write_waker: Mutex::new(None),
    });
    let task = TaskQueue { state, handle };
    let thread = ThreadQueue(Arc::clone(&task.state));
    (task, thread)
}

#[derive(thiserror::Error, Debug)]
pub enum TaskError {
    #[error("io error => {0}")]
    Io(#[from] io::Error),
    #[error("failed to send data to thread, the queue is full")]
    Overflow(BytesMut),
}

/// Shared state between the task and the thread
#[derive(Debug)]
struct State {
    /// The queue consumed by the task
    task: ArrayQueue<Option<io::Result<BytesMut>>>,
    /// The queue consumed by the thread
    thread: ArrayQueue<Option<BytesMut>>,
    /// Let the task know its time to read more bytes
    read_waker: Mutex<Option<Waker>>,
    /// Let the task know its ok to write more bytes
    write_waker: Mutex<Option<Waker>>,
    // TODO need `event` to let the thread know its ok to send more bytes
}

/// TODO get rid of generic, use a mock wake handle or a real RawHandle impl to CancelIo
/// TODO think about race condition via breaking a read loop. Read thread + Write thread? or Async?
/// or some how manage read / write in same thread
pub struct TaskQueue<W> {
    state: Arc<State>,
    handle: W,
}

impl<W: WakeHandle> TaskQueue<W> {
    /// Push data to the thread side of the queue
    /// TODO deprecate this infavor of AsyncWrite implementation (supports throttle w/ poll api)
    pub fn push(&self, bytes: BytesMut) -> Result<(), TaskError> {
        self.state
            .thread
            .push(Some(bytes))
            .map_err(|bytes| match bytes {
                Some(bytes) => TaskError::Overflow(bytes),
                _ => unreachable!(),
            })?;
        self.handle.wake().map_err(TaskError::from)
    }

    /// TODO deprecate (use AsyncRead)
    pub fn listen(&self) -> TaskStream {
        TaskStream(Arc::clone(&self.state))
    }

    /// TODO deprecate, TaskQueue should implement AsyncRead and AsyncWrite
    pub fn reader(&self) -> Reader {
        Reader::from(self.listen())
    }

    /// TODO deprecate TaskQueue should implement AsyncRead and AsyncWrite
    pub fn writer(&self) -> Writer {
        Writer(Arc::clone(&self.state))
    }
}

impl<W> Drop for TaskQueue<W> {
    fn drop(&mut self) {
        self.state.thread.force_push(None);
    }
}

#[derive(Clone)]
pub struct ThreadQueue(Arc<State>);

impl ThreadQueue {
    /// Push data to the task side of the queue
    pub fn push_ok(&self, bytes: BytesMut) -> Result<(), BytesMut> {
        match self.0.task.push(Some(Ok(bytes))) {
            Err(Some(Ok(bytes))) => Err(bytes),
            Err(_) => unreachable!(),
            Ok(_) => match self.0.read_waker.lock().as_ref() {
                None => Ok(()),
                Some(waker) => {
                    waker.wake_by_ref();
                    Ok(())
                }
            },
        }
    }

    /// Push data to the task side of the queue
    pub fn push_err(&self, err: io::Error) -> Result<(), io::Error> {
        match self.0.task.push(Some(Err(err))) {
            Err(Some(Err(e))) => Err(e),
            Err(_) => unreachable!(),
            Ok(_) => match self.0.read_waker.lock().as_ref() {
                None => Ok(()),
                Some(waker) => {
                    waker.wake_by_ref();
                    Ok(())
                }
            },
        }
    }

    /// Thread side consumer
    pub fn pop(&self) -> Option<Option<BytesMut>> {
        if self.0.thread.len() > 0 {
            if let Some(waker) = self.0.write_waker.lock().as_ref() {
                waker.wake_by_ref();
            }
            self.0.thread.pop()
        } else {
            None
        }
    }

    /// Collect all the bytes into a single buffer
    pub fn collect(&self) -> (BytesMut, bool) {
        let mut ret = BytesMut::new();
        let mut done = false;
        while let Some(stream) = self.0.thread.pop() {
            match stream {
                Some(bytes) => ret.put(bytes),
                None => {
                    done = true;
                    break;
                }
            }
        }
        if ret.len() > 0 {
            if let Some(waker) = self.0.write_waker.lock().as_ref() {
                waker.wake_by_ref();
            }
        }
        (ret, done)
    }
}

impl Drop for ThreadQueue {
    fn drop(&mut self) {
        self.0.task.force_push(None);
    }
}

#[derive(Clone, Debug)]
pub struct TaskStream(Arc<State>);
impl Stream for TaskStream {
    type Item = io::Result<BytesMut>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.task.pop() {
            Some(item) => Poll::Ready(item),
            None => {
                let mut waker = self.0.read_waker.lock();
                let new_waker = cx.waker();
                *waker = match waker.take() {
                    None => Some(new_waker.clone()),
                    Some(old_waker) => match old_waker.will_wake(cx.waker()) {
                        false => Some(new_waker.clone()),
                        true => Some(old_waker),
                    },
                };
                Poll::Pending
            }
        }
    }
}

pin_project! {
    #[project = ReaderProj]
    #[project_replace = ReaderProjReplace]
    #[derive(Debug)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub enum Reader {
        Incomplete {
            #[pin]
            inner: TaskStream,
            current: Option<io::Result<BytesMut>>,
        },
        Complete,
    }
}

impl From<TaskStream> for Reader {
    fn from(inner: TaskStream) -> Self {
        Reader::Incomplete {
            inner,
            current: None,
        }
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut idx = 0usize;
        match self.as_mut().project() {
            ReaderProj::Incomplete { mut inner, current } => loop {
                match current {
                    None => match futures::ready!(inner.as_mut().poll_next(cx)) {
                        None => {
                            self.project_replace(Reader::Complete);
                            break Poll::Ready(Ok(idx));
                        }
                        Some(next) => current.replace(next).map_or_else(|| (), |_| ()),
                    },
                    Some(Err(_)) if idx > 0 => break Poll::Ready(Ok(idx)),
                    Some(Err(_)) => match current.take() {
                        Some(Err(e)) => break Poll::Ready(Err(e)),
                        _ => unreachable!(),
                    },
                    Some(Ok(next)) => {
                        let remaining = buf.len() - idx;
                        if next.len() <= remaining {
                            buf[idx..idx + next.len()].copy_from_slice(&next);
                            idx = idx + next.len();
                            next.advance(next.len());
                            // loop around for next queue item
                            if next.len() == 0 {
                                current.take();
                            }
                        } else {
                            buf[idx..idx + remaining].copy_from_slice(&next[..remaining]);
                            next.advance(remaining);
                            idx += remaining;
                            // We filled the callers buffer
                            break Poll::Ready(Ok(idx));
                        }
                    }
                }
            },
            ReaderProj::Complete => Poll::Ready(Ok(0)),
        }
    }
}

/// TODO impl on TaskQueue which has access to WakeHandle
pub struct Writer(Arc<State>);
impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // TODO the writer needs the handle to call wake
        match self.0.thread.push(Some(BytesMut::from(buf))) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(_bytes) => {
                let mut waker = self.0.write_waker.lock();
                let new_waker = cx.waker();
                *waker = match waker.take() {
                    None => Some(new_waker.clone()),
                    Some(old_waker) => match old_waker.will_wake(cx.waker()) {
                        false => Some(new_waker.clone()),
                        true => Some(old_waker),
                    },
                };
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.0.thread.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            let mut waker = self.0.write_waker.lock();
            let new_waker = cx.waker();
            *waker = match waker.take() {
                None => Some(new_waker.clone()),
                Some(old_waker) => match old_waker.will_wake(cx.waker()) {
                    false => Some(new_waker.clone()),
                    true => Some(old_waker),
                },
            };
            Poll::Pending
        }
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.thread.force_push(None);
        Poll::Ready(Ok(()))
    }
}
