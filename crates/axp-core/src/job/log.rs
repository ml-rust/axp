//! Append-only, replayable log buffer for a job's stdout/stderr output.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;

use crate::{Error, Result};

/// Monotonic sequence number within one job's log stream (starts at 0, equals buffer index).
pub type Seq = u64;

/// Which standard stream a log chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One ordered log chunk from a job. Raw bytes — NO line-splitting at this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// Monotonic position of this event within the job's log stream.
    pub seq: Seq,
    /// Which standard stream the bytes came from.
    pub stream: LogStream,
    /// The raw bytes of this chunk.
    pub data: Bytes,
    /// Wall-clock time at which the chunk was recorded.
    pub timestamp: SystemTime,
}

/// Default log-buffer byte cap: 64 MiB.
pub const DEFAULT_LOG_BYTE_CAP: usize = 64 * 1024 * 1024;

/// An append-only, replayable buffer of a job's log events.
///
/// `seq == index`, so [`since`](LogBuffer::since) is O(1) slicing and a
/// re-attaching subscriber can replay everything from any offset.
///
/// The buffer is bounded by a byte cap; exceeding it returns
/// [`Error::LogBufferOverflow`].  Output is **never** silently dropped — the
/// engine will kill the job and mark it `Failed` on overflow.
#[derive(Debug)]
pub struct LogBuffer {
    events: Vec<LogEvent>,
    byte_total: usize,
    byte_cap: usize,
    /// Wake signal fired after each successful push so live subscribers re-read
    /// via [`since`](LogBuffer::since). Shared with every [`subscribe`](LogBuffer::subscribe) handle.
    notify: Arc<tokio::sync::Notify>,
}

impl LogBuffer {
    /// Create a new buffer with the [`DEFAULT_LOG_BYTE_CAP`].
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_LOG_BYTE_CAP)
    }

    /// Create a new buffer with an explicit byte cap.
    pub fn with_cap(byte_cap: usize) -> Self {
        Self {
            events: Vec::new(),
            byte_total: 0,
            byte_cap,
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Append a chunk to the buffer.
    ///
    /// Returns the assigned [`Seq`] on success.  Returns
    /// [`Error::LogBufferOverflow`] if adding `data` would exceed the byte cap;
    /// in that case the buffer is left unchanged.
    pub fn push(&mut self, stream: LogStream, data: Bytes, timestamp: SystemTime) -> Result<Seq> {
        if self.byte_total + data.len() > self.byte_cap {
            return Err(Error::LogBufferOverflow { cap: self.byte_cap });
        }
        let seq = self.events.len() as Seq;
        self.byte_total += data.len();
        self.events.push(LogEvent {
            seq,
            stream,
            data,
            timestamp,
        });
        self.notify.notify_waiters();
        Ok(seq)
    }

    /// Return a handle to this buffer's wake signal.
    ///
    /// The signal is fired after each successful [`push`](LogBuffer::push);
    /// subscribers await it and then re-read new events via
    /// [`since`](LogBuffer::since). All handles share the same underlying
    /// [`tokio::sync::Notify`].
    pub fn subscribe(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    /// Return all events with `seq >= from_seq`.
    ///
    /// If `from_seq` is past the end of the buffer an empty slice is returned
    /// (no panic).
    pub fn since(&self, from_seq: Seq) -> &[LogEvent] {
        let start = (from_seq as usize).min(self.events.len());
        &self.events[start..]
    }

    /// The number of events currently held in the buffer.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` if the buffer holds no events.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        SystemTime::now()
    }

    fn push_chunk(buf: &mut LogBuffer, data: &[u8]) -> Result<Seq> {
        buf.push(LogStream::Stdout, Bytes::copy_from_slice(data), now())
    }

    #[test]
    fn push_five_events_assigns_seqs_zero_to_four() {
        let mut buf = LogBuffer::new();
        for i in 0u64..5 {
            let seq = push_chunk(&mut buf, b"x").unwrap();
            assert_eq!(seq, i);
        }
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn since_returns_suffix_slice() {
        let mut buf = LogBuffer::new();
        for _ in 0..5 {
            push_chunk(&mut buf, b"y").unwrap();
        }
        let slice = buf.since(2);
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0].seq, 2);
        assert_eq!(slice[1].seq, 3);
        assert_eq!(slice[2].seq, 4);
    }

    #[test]
    fn since_beyond_end_returns_empty_no_panic() {
        let mut buf = LogBuffer::new();
        push_chunk(&mut buf, b"z").unwrap();
        let slice = buf.since(99);
        assert!(slice.is_empty());
    }

    #[test]
    fn with_cap_small_within_cap_succeeds() {
        let mut buf = LogBuffer::with_cap(10);
        let seq = push_chunk(&mut buf, b"hello").unwrap(); // 5 bytes
        assert_eq!(seq, 0);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn with_cap_small_overflow_returns_error_and_does_not_append() {
        let mut buf = LogBuffer::with_cap(4);
        push_chunk(&mut buf, b"ab").unwrap(); // 2 bytes, ok
        // Next push would bring total to 2 + 3 = 5 > 4: overflow.
        let result = push_chunk(&mut buf, b"cde");
        assert!(
            matches!(result, Err(Error::LogBufferOverflow { cap: 4 })),
            "expected LogBufferOverflow(cap=4), got {result:?}"
        );
        // The failed push must NOT have been appended.
        assert_eq!(buf.len(), 1, "event must not be appended on overflow");
    }

    #[test]
    fn subscribe_returns_a_usable_handle() {
        let mut buf = LogBuffer::new();
        push_chunk(&mut buf, b"x").unwrap();
        let handle = buf.subscribe();
        // The handle is a usable Notify; a second handle points at the same one.
        let handle2 = buf.subscribe();
        assert!(Arc::ptr_eq(&handle, &handle2));
    }
}
