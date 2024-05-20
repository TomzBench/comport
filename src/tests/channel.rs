//! channel

use crate::channel::{self, WakeHandle};
use bytes::BytesMut;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt};
use std::{
    io,
    os::windows::io::{AsRawHandle, RawHandle},
    pin::pin,
    task::Poll,
};

struct MockHandle {}
impl AsRawHandle for MockHandle {
    fn as_raw_handle(&self) -> RawHandle {
        0 as _
    }
}

impl WakeHandle for MockHandle {
    fn wake(&self) -> std::io::Result<()> {
        Ok(())
    }
}

macro_rules! assert_ready_eq {
    ($expect:expr, $poll:expr) => {
        match $poll {
            Poll::Ready(Some(Ok(bytes))) => assert_eq!($expect, bytes),
            _ => panic!("unexpected poll"),
        }
    };
}

macro_rules! assert_ready {
    ($poll:expr) => {
        match $poll {
            Poll::Ready(item) => item,
            _ => panic!("unexpected poll"),
        }
    };
}

#[test]
fn comport_test_channel_task() {
    // Create a test waker
    let waker = futures::task::noop_waker_ref();
    let mut cx = std::task::Context::from_waker(waker);

    let handle = MockHandle {};
    let (task, thread) = channel::bounded(handle, 4);

    let mut stream = task.listen();

    // Assure our stream is empty
    let poll = stream.poll_next_unpin(&mut cx);
    assert!(!poll.is_ready());

    // Push data from thread side to task side
    let bytes = BytesMut::from("hi");
    thread.push_ok(bytes.clone()).unwrap();
    let poll = stream.poll_next_unpin(&mut cx);
    assert_ready_eq!(bytes, poll);

    // Ensure closing stream
    drop(thread);
    let poll = assert_ready!(stream.poll_next_unpin(&mut cx));
    assert!(poll.is_none());
}

#[test]
fn comport_test_channel_thread() {
    // TODO use mockall and assert our handle is waking
    let handle = MockHandle {};
    let (task, thread) = channel::bounded(handle, 4);

    // Assure our queue is empty
    assert_eq!(None, thread.pop());

    // push data from the task side to the thread side
    let bytes = BytesMut::from("hi");
    task.push(bytes.clone()).unwrap();
    assert_eq!(Some(Some(bytes)), thread.pop());

    // Ensure closing
    drop(task);
    assert_eq!(Some(None), thread.pop());
}

#[tokio::test]
async fn comport_test_channel_thread_collect() {
    // Create a test waker
    let waker = futures::task::noop_waker_ref();
    let mut cx = std::task::Context::from_waker(waker);

    let handle = MockHandle {};
    let (task, thread) = channel::bounded(handle, 2);

    let mut writer = pin!(task.writer());

    writer.write(b"hello").await.unwrap();
    writer.write(b"world").await.unwrap();

    // Make sure we throttle outgoing bytes
    let poll = writer.as_mut().poll_flush(&mut cx);
    assert!(poll.is_pending());

    // Collect all the bytes make sure still more
    let (bytes, done) = thread.collect();
    assert!(!done);
    assert_eq!("helloworld", bytes);

    // Make sure not pending anymore
    let poll = writer.as_mut().poll_flush(&mut cx);
    assert!(poll.is_ready());

    writer.write(b"ab").await.unwrap();
    drop(task);

    // Collect all the bytes make sure signal done
    let (bytes, done) = thread.collect();
    assert!(done);
    assert_eq!("ab", bytes);
}

#[tokio::test]
async fn comport_test_channel_reader() {
    // Create a test waker
    let waker = futures::task::noop_waker_ref();
    let mut cx = std::task::Context::from_waker(waker);

    let handle = MockHandle {};
    let (task, thread) = channel::bounded(handle, 14);

    // Make sure we are pending
    let mut buf = [0; 21];
    let mut reader = pin!(task.reader());
    let poll = reader.as_mut().poll_read(&mut cx, &mut buf);
    assert!(poll.is_pending());

    // Prepare some bytes to read
    let error = io::Error::new(io::ErrorKind::Other, "test error");
    thread.push_ok(BytesMut::from("hello")).unwrap();
    thread.push_ok(BytesMut::from("world")).unwrap();
    thread.push_ok(BytesMut::from("a")).unwrap();
    thread.push_ok(BytesMut::from("b")).unwrap();
    thread.push_ok(BytesMut::from("c")).unwrap();
    thread.push_ok(BytesMut::from("d")).unwrap();
    thread.push_ok(BytesMut::from("e")).unwrap();
    thread.push_ok(BytesMut::from("f")).unwrap();
    thread.push_ok(BytesMut::from("g")).unwrap();
    thread.push_err(error).unwrap();
    thread.push_ok(BytesMut::from("0")).unwrap();
    thread.push_ok(BytesMut::from("1")).unwrap();
    thread.push_ok(BytesMut::from("2")).unwrap();
    drop(thread);

    // Read a partial
    let read = reader.read(&mut buf[0..2]).await.unwrap();
    assert_eq!(2, read);
    assert_eq!("he".as_bytes(), &buf[0..2]);

    // Make sure we loop to next queue item
    let read = reader.read(&mut buf[3..9]).await.unwrap();
    assert_eq!(6, read);
    assert_eq!("llowor".as_bytes(), &buf[3..9]);

    // Read rest of world
    let read = reader.read(&mut buf[9..11]).await.unwrap();
    assert_eq!(2, read);
    assert_eq!("ld".as_bytes(), &buf[9..11]);

    // Make sure we loop around until buffer is full
    let read = reader.read(&mut buf[11..]).await.unwrap();
    assert_eq!(7, read);
    assert_eq!("abcdefg".as_bytes(), &buf[11..18]);

    // Make sure we read the error
    let read = reader.read(&mut buf[18..]).await;
    assert!(read.is_err());

    // Make sure we read past the error
    let read = reader.read(&mut buf[18..]).await.unwrap();
    assert_eq!(3, read);
    assert_eq!("012".as_bytes(), &buf[18..21]);

    // eof
    let read = reader.read(&mut buf).await.unwrap();
    assert_eq!(0, read);

    // fused
    let read = reader.read(&mut buf).await.unwrap();
    assert_eq!(0, read);
}

#[tokio::test]
async fn comport_test_channel_writer() {
    // Create a test waker
    let waker = futures::task::noop_waker_ref();
    let mut cx = std::task::Context::from_waker(waker);

    let handle = MockHandle {};
    let (task, thread) = channel::bounded(handle, 2);

    // Write some bytes
    let mut writer = pin!(task.writer());
    let write = writer.write(b"hello").await.unwrap();
    assert_eq!(5, write);

    // Fill our outgoing queue
    let write = writer.write(b"world").await.unwrap();
    assert_eq!(5, write);

    // Make sure we throttle outgoing bytes
    let poll = writer.as_mut().poll_flush(&mut cx);
    assert!(poll.is_pending());

    // Free up queue space
    let recvd = thread.pop().unwrap().unwrap();
    assert_eq!("hello", recvd);
    let poll = writer.as_mut().poll_flush(&mut cx);
    assert!(poll.is_pending());

    // Flush complete
    let recvd = thread.pop().unwrap().unwrap();
    assert_eq!("world", recvd);
    let poll = writer.as_mut().poll_flush(&mut cx);
    assert!(poll.is_ready());
}
