//! Append-only, replayable job log for stdout/stderr output.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// A log event before the replay log assigns its monotonic sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendLogEvent {
    /// Which standard stream the bytes came from.
    pub stream: LogStream,
    /// The raw bytes of this chunk.
    pub data: Bytes,
    /// Wall-clock time at which the chunk was recorded.
    pub timestamp: SystemTime,
}

/// Default log-buffer byte cap: 64 MiB.
pub const DEFAULT_LOG_BYTE_CAP: usize = 64 * 1024 * 1024;

const FILE_LOG_MAGIC: &[u8] = b"AXPJOBLOG1\n";
const FILE_LOG_HEADER_LEN: u64 = FILE_LOG_MAGIC.len() as u64;
const FILE_LOG_FRAME_FIXED_LEN: usize = 8 + 1 + 8 + 4 + 8;
const STREAM_STDOUT: u8 = 1;
const STREAM_STDERR: u8 = 2;

/// Replay result returned by a [`JobReplayLog`].
///
/// The current in-memory implementation returns a borrowed slice. Durable
/// implementations can return an owned batch without changing attach callers.
#[derive(Debug, Clone)]
pub struct LogReplay<'a> {
    events: Cow<'a, [LogEvent]>,
}

impl<'a> LogReplay<'a> {
    /// Build a replay view over borrowed events.
    pub fn borrowed(events: &'a [LogEvent]) -> Self {
        Self {
            events: Cow::Borrowed(events),
        }
    }

    /// Build a replay view over owned events.
    pub fn owned(events: Vec<LogEvent>) -> Self {
        Self {
            events: Cow::Owned(events),
        }
    }

    /// Return replayed events in sequence order.
    pub fn events(&self) -> &[LogEvent] {
        self.events.as_ref()
    }

    /// Returns `true` if no events are available from the requested cursor.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl<'a, 'b> IntoIterator for &'b LogReplay<'a> {
    type Item = &'b LogEvent;
    type IntoIter = std::slice::Iter<'b, LogEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.events().iter()
    }
}

/// Append/replay boundary for a job's ordered log events.
///
/// Implementations assign `seq == offset` for every successful append, preserve
/// append order for replay, and must leave existing data unchanged when an
/// append fails. Wake handles are signalled after successful appends and may also
/// be signalled by job lifecycle code when terminal state changes without a log
/// event.
pub trait JobReplayLog: std::fmt::Debug + Send + Sync {
    /// Append an event and return its assigned sequence number.
    fn append(&mut self, event: AppendLogEvent) -> Result<Seq>;

    /// Return all events with `seq >= from_seq` in ascending sequence order.
    fn replay_from(&self, from_seq: Seq) -> LogReplay<'_>;

    /// Return a handle to this log's wake signal.
    fn subscribe(&self) -> Arc<tokio::sync::Notify>;

    /// The next sequence number that will be assigned by a successful append.
    fn next_seq(&self) -> Seq;

    /// The number of events currently available for replay.
    fn len(&self) -> usize;

    /// Returns `true` if the log holds no events.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory implementation of [`JobReplayLog`].
///
/// `seq == index`, so replay is O(1) slicing and a re-attaching subscriber can
/// replay everything from any offset.
#[derive(Debug)]
pub struct InMemoryJobReplayLog {
    events: Vec<LogEvent>,
    byte_total: usize,
    byte_cap: usize,
    notify: Arc<tokio::sync::Notify>,
}

impl InMemoryJobReplayLog {
    /// Create a new in-memory replay log with the [`DEFAULT_LOG_BYTE_CAP`].
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_LOG_BYTE_CAP)
    }

    /// Create a new in-memory replay log with an explicit byte cap.
    pub fn with_cap(byte_cap: usize) -> Self {
        Self {
            events: Vec::new(),
            byte_total: 0,
            byte_cap,
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Return all events with `seq >= from_seq` as a borrowed slice.
    ///
    /// If `from_seq` is past the end of the log an empty slice is returned.
    pub fn since(&self, from_seq: Seq) -> &[LogEvent] {
        let start = (from_seq as usize).min(self.events.len());
        &self.events[start..]
    }
}

impl Default for InMemoryJobReplayLog {
    fn default() -> Self {
        Self::new()
    }
}

impl JobReplayLog for InMemoryJobReplayLog {
    fn append(&mut self, event: AppendLogEvent) -> Result<Seq> {
        let new_total = self
            .byte_total
            .checked_add(event.data.len())
            .ok_or(Error::LogBufferOverflow { cap: self.byte_cap })?;
        if new_total > self.byte_cap {
            return Err(Error::LogBufferOverflow { cap: self.byte_cap });
        }
        let seq = self.next_seq();
        self.byte_total = new_total;
        self.events.push(LogEvent {
            seq,
            stream: event.stream,
            data: event.data,
            timestamp: event.timestamp,
        });
        self.notify.notify_waiters();
        Ok(seq)
    }

    fn replay_from(&self, from_seq: Seq) -> LogReplay<'_> {
        LogReplay::borrowed(self.since(from_seq))
    }

    fn subscribe(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    fn next_seq(&self) -> Seq {
        self.events.len() as Seq
    }

    fn len(&self) -> usize {
        self.events.len()
    }
}

/// File-backed implementation of [`JobReplayLog`].
///
/// The on-disk format is an append-only binary stream:
///
/// - magic header: `AXPJOBLOG1\n`
/// - repeated frames: `seq:u64`, `stream:u8`, `timestamp_secs:i64`,
///   `timestamp_nanos:u32`, `data_len:u64`, then `data_len` raw bytes.
///
/// Opening a log validates every complete frame, including sequence continuity.
/// Any truncated frame, invalid stream tag, invalid timestamp, or sequence gap is
/// reported as corruption rather than silently ignored.
#[derive(Debug)]
pub struct FileJobReplayLog {
    path: PathBuf,
    file: File,
    events: Vec<LogEvent>,
    byte_total: usize,
    byte_cap: usize,
    notify: Arc<tokio::sync::Notify>,
}

impl FileJobReplayLog {
    /// Open or create a file-backed replay log with the [`DEFAULT_LOG_BYTE_CAP`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_cap(path, DEFAULT_LOG_BYTE_CAP)
    }

    /// Open or create a file-backed replay log with an explicit byte cap.
    pub fn with_cap(path: impl AsRef<Path>, byte_cap: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| Error::ReplayLogIo {
                path: path.clone(),
                source,
            })?;

        if file
            .metadata()
            .map(|metadata| metadata.len())
            .map_err(|source| Error::ReplayLogIo {
                path: path.clone(),
                source,
            })?
            == 0
        {
            file.write_all(FILE_LOG_MAGIC)
                .and_then(|()| file.sync_all())
                .map_err(|source| Error::ReplayLogIo {
                    path: path.clone(),
                    source,
                })?;
        }

        file.seek(SeekFrom::Start(0))
            .map_err(|source| Error::ReplayLogIo {
                path: path.clone(),
                source,
            })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::ReplayLogIo {
                path: path.clone(),
                source,
            })?;
        let (events, byte_total) = decode_file_log(&path, &bytes, byte_cap)?;

        file.seek(SeekFrom::End(0))
            .map_err(|source| Error::ReplayLogIo {
                path: path.clone(),
                source,
            })?;

        Ok(Self {
            path,
            file,
            events,
            byte_total,
            byte_cap,
            notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Return all events with `seq >= from_seq` as a borrowed slice.
    ///
    /// If `from_seq` is past the end of the log an empty slice is returned.
    pub fn since(&self, from_seq: Seq) -> &[LogEvent] {
        let start = (from_seq as usize).min(self.events.len());
        &self.events[start..]
    }
}

impl JobReplayLog for FileJobReplayLog {
    fn append(&mut self, event: AppendLogEvent) -> Result<Seq> {
        let new_total = self
            .byte_total
            .checked_add(event.data.len())
            .ok_or(Error::LogBufferOverflow { cap: self.byte_cap })?;
        if new_total > self.byte_cap {
            return Err(Error::LogBufferOverflow { cap: self.byte_cap });
        }

        let seq = seq_from_len(self.events.len(), &self.path, self.events.len() as u64)?;
        let frame = encode_frame(&self.path, seq, &event)?;
        self.file
            .write_all(&frame)
            .and_then(|()| self.file.sync_data())
            .map_err(|source| Error::ReplayLogIo {
                path: self.path.clone(),
                source,
            })?;

        self.byte_total = new_total;
        self.events.push(LogEvent {
            seq,
            stream: event.stream,
            data: event.data,
            timestamp: event.timestamp,
        });
        self.notify.notify_waiters();
        Ok(seq)
    }

    fn replay_from(&self, from_seq: Seq) -> LogReplay<'_> {
        LogReplay::borrowed(self.since(from_seq))
    }

    fn subscribe(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    fn next_seq(&self) -> Seq {
        self.events.len() as Seq
    }

    fn len(&self) -> usize {
        self.events.len()
    }
}

/// An append-only, replayable buffer of a job's log events.
///
/// This is the repo-native in-memory facade used by current callers. The
/// durable-facing abstraction is [`JobReplayLog`]; this type preserves the
/// existing `LogBuffer` API while delegating to [`InMemoryJobReplayLog`].
///
/// `seq == index`, so [`since`](LogBuffer::since) is O(1) slicing and a
/// re-attaching subscriber can replay everything from any offset.
///
/// The buffer is bounded by a byte cap; exceeding it returns
/// [`Error::LogBufferOverflow`].  Output is **never** silently dropped — the
/// engine will kill the job and mark it `Failed` on overflow.
#[derive(Debug, Default)]
pub struct LogBuffer {
    inner: InMemoryJobReplayLog,
}

impl LogBuffer {
    /// Create a new buffer with the [`DEFAULT_LOG_BYTE_CAP`].
    pub fn new() -> Self {
        Self {
            inner: InMemoryJobReplayLog::new(),
        }
    }

    /// Create a new buffer with an explicit byte cap.
    pub fn with_cap(byte_cap: usize) -> Self {
        Self {
            inner: InMemoryJobReplayLog::with_cap(byte_cap),
        }
    }

    /// Append a chunk to the buffer.
    ///
    /// Returns the assigned [`Seq`] on success.  Returns
    /// [`Error::LogBufferOverflow`] if adding `data` would exceed the byte cap;
    /// in that case the buffer is left unchanged.
    pub fn push(&mut self, stream: LogStream, data: Bytes, timestamp: SystemTime) -> Result<Seq> {
        self.append(AppendLogEvent {
            stream,
            data,
            timestamp,
        })
    }

    /// Return a handle to this buffer's wake signal.
    ///
    /// The signal is fired after each successful [`push`](LogBuffer::push);
    /// subscribers await it and then re-read new events via
    /// [`since`](LogBuffer::since). All handles share the same underlying
    /// [`tokio::sync::Notify`].
    pub fn subscribe(&self) -> Arc<tokio::sync::Notify> {
        self.inner.subscribe()
    }

    /// Return all events with `seq >= from_seq`.
    ///
    /// If `from_seq` is past the end of the buffer an empty slice is returned
    /// (no panic).
    pub fn since(&self, from_seq: Seq) -> &[LogEvent] {
        self.inner.since(from_seq)
    }

    /// Return all events with `seq >= from_seq` as a replay view.
    pub fn replay_from(&self, from_seq: Seq) -> LogReplay<'_> {
        self.inner.replay_from(from_seq)
    }

    /// The number of events currently held in the buffer.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the buffer holds no events.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl JobReplayLog for LogBuffer {
    fn append(&mut self, event: AppendLogEvent) -> Result<Seq> {
        self.inner.append(event)
    }

    fn replay_from(&self, from_seq: Seq) -> LogReplay<'_> {
        self.inner.replay_from(from_seq)
    }

    fn subscribe(&self) -> Arc<tokio::sync::Notify> {
        self.inner.subscribe()
    }

    fn next_seq(&self) -> Seq {
        self.inner.next_seq()
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

fn seq_from_len(len: usize, path: &Path, offset: u64) -> Result<Seq> {
    Seq::try_from(len).map_err(|_| Error::ReplayLogCorrupt {
        path: path.to_path_buf(),
        offset,
        reason: "event count exceeds sequence range".to_string(),
    })
}

fn encode_frame(path: &Path, seq: Seq, event: &AppendLogEvent) -> Result<Vec<u8>> {
    let (secs, nanos) = encode_timestamp(event.timestamp, path, 0)?;
    let data_len = u64::try_from(event.data.len()).map_err(|_| Error::ReplayLogCorrupt {
        path: path.to_path_buf(),
        offset: 0,
        reason: "log event data length exceeds frame range".to_string(),
    })?;
    let frame_cap = FILE_LOG_FRAME_FIXED_LEN
        .checked_add(event.data.len())
        .ok_or_else(|| Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset: 0,
            reason: "frame length exceeds addressable memory".to_string(),
        })?;
    let mut frame = Vec::with_capacity(frame_cap);
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.push(encode_stream(event.stream));
    frame.extend_from_slice(&secs.to_le_bytes());
    frame.extend_from_slice(&nanos.to_le_bytes());
    frame.extend_from_slice(&data_len.to_le_bytes());
    frame.extend_from_slice(&event.data);
    Ok(frame)
}

fn decode_file_log(path: &Path, bytes: &[u8], byte_cap: usize) -> Result<(Vec<LogEvent>, usize)> {
    if !bytes.starts_with(FILE_LOG_MAGIC) {
        return Err(Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset: 0,
            reason: "missing replay log magic header".to_string(),
        });
    }

    let mut offset = FILE_LOG_HEADER_LEN as usize;
    let mut events = Vec::new();
    let mut byte_total = 0usize;

    while offset < bytes.len() {
        let frame_start = offset;
        let seq = read_u64(path, bytes, &mut offset)?;
        let stream = decode_stream(read_u8(path, bytes, &mut offset)?, path, frame_start as u64)?;
        let secs = read_i64(path, bytes, &mut offset)?;
        let nanos = read_u32(path, bytes, &mut offset)?;
        let data_len = read_u64(path, bytes, &mut offset)?;
        let data_len_usize = usize::try_from(data_len).map_err(|_| Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset: frame_start as u64,
            reason: "frame data length exceeds addressable memory".to_string(),
        })?;
        let data_end =
            offset
                .checked_add(data_len_usize)
                .ok_or_else(|| Error::ReplayLogCorrupt {
                    path: path.to_path_buf(),
                    offset: frame_start as u64,
                    reason: "frame data length overflows file offset".to_string(),
                })?;
        if data_end > bytes.len() {
            return Err(Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset: offset as u64,
                reason: "truncated frame data".to_string(),
            });
        }
        let expected_seq = seq_from_len(events.len(), path, frame_start as u64)?;
        if seq != expected_seq {
            return Err(Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset: frame_start as u64,
                reason: format!("expected seq {expected_seq}, found {seq}"),
            });
        }
        byte_total = byte_total
            .checked_add(data_len_usize)
            .ok_or(Error::LogBufferOverflow { cap: byte_cap })?;
        if byte_total > byte_cap {
            return Err(Error::LogBufferOverflow { cap: byte_cap });
        }
        events.push(LogEvent {
            seq,
            stream,
            data: Bytes::copy_from_slice(&bytes[offset..data_end]),
            timestamp: decode_timestamp(secs, nanos, path, frame_start as u64)?,
        });
        offset = data_end;
    }

    Ok((events, byte_total))
}

fn read_u8(path: &Path, bytes: &[u8], offset: &mut usize) -> Result<u8> {
    let value = *bytes.get(*offset).ok_or_else(|| Error::ReplayLogCorrupt {
        path: path.to_path_buf(),
        offset: *offset as u64,
        reason: "truncated frame".to_string(),
    })?;
    *offset += 1;
    Ok(value)
}

fn read_u32(path: &Path, bytes: &[u8], offset: &mut usize) -> Result<u32> {
    let end = checked_read_end(path, bytes, *offset, 4)?;
    let mut raw = [0; 4];
    raw.copy_from_slice(&bytes[*offset..end]);
    *offset = end;
    Ok(u32::from_le_bytes(raw))
}

fn read_i64(path: &Path, bytes: &[u8], offset: &mut usize) -> Result<i64> {
    let end = checked_read_end(path, bytes, *offset, 8)?;
    let mut raw = [0; 8];
    raw.copy_from_slice(&bytes[*offset..end]);
    *offset = end;
    Ok(i64::from_le_bytes(raw))
}

fn read_u64(path: &Path, bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let end = checked_read_end(path, bytes, *offset, 8)?;
    let mut raw = [0; 8];
    raw.copy_from_slice(&bytes[*offset..end]);
    *offset = end;
    Ok(u64::from_le_bytes(raw))
}

fn checked_read_end(path: &Path, bytes: &[u8], offset: usize, len: usize) -> Result<usize> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset: offset as u64,
            reason: "frame offset overflow".to_string(),
        })?;
    if end > bytes.len() {
        return Err(Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset: offset as u64,
            reason: "truncated frame".to_string(),
        });
    }
    Ok(end)
}

fn encode_stream(stream: LogStream) -> u8 {
    match stream {
        LogStream::Stdout => STREAM_STDOUT,
        LogStream::Stderr => STREAM_STDERR,
    }
}

fn decode_stream(tag: u8, path: &Path, offset: u64) -> Result<LogStream> {
    match tag {
        STREAM_STDOUT => Ok(LogStream::Stdout),
        STREAM_STDERR => Ok(LogStream::Stderr),
        _ => Err(Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset,
            reason: format!("invalid stream tag {tag}"),
        }),
    }
}

fn encode_timestamp(timestamp: SystemTime, path: &Path, offset: u64) -> Result<(i64, u32)> {
    match timestamp.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset,
                reason: "timestamp seconds exceed frame range".to_string(),
            })?;
            Ok((secs, duration.subsec_nanos()))
        }
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset,
                reason: "timestamp seconds exceed frame range".to_string(),
            })?;
            if duration.subsec_nanos() == 0 {
                Ok((-secs, 0))
            } else {
                let shifted_secs = secs.checked_add(1).ok_or_else(|| Error::ReplayLogCorrupt {
                    path: path.to_path_buf(),
                    offset,
                    reason: "timestamp seconds exceed frame range".to_string(),
                })?;
                Ok((-shifted_secs, 1_000_000_000 - duration.subsec_nanos()))
            }
        }
    }
}

fn decode_timestamp(secs: i64, nanos: u32, path: &Path, offset: u64) -> Result<SystemTime> {
    if nanos >= 1_000_000_000 {
        return Err(Error::ReplayLogCorrupt {
            path: path.to_path_buf(),
            offset,
            reason: format!("invalid timestamp nanos {nanos}"),
        });
    }

    if secs >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::new(secs as u64, nanos))
            .ok_or_else(|| Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset,
                reason: "timestamp exceeds system time range".to_string(),
            })
    } else if nanos == 0 {
        UNIX_EPOCH
            .checked_sub(Duration::new(secs.unsigned_abs(), 0))
            .ok_or_else(|| Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset,
                reason: "timestamp exceeds system time range".to_string(),
            })
    } else {
        let secs_before_epoch =
            secs.unsigned_abs()
                .checked_sub(1)
                .ok_or_else(|| Error::ReplayLogCorrupt {
                    path: path.to_path_buf(),
                    offset,
                    reason: "timestamp exceeds system time range".to_string(),
                })?;
        let nanos_before_epoch = 1_000_000_000 - nanos;
        UNIX_EPOCH
            .checked_sub(Duration::new(secs_before_epoch, nanos_before_epoch))
            .ok_or_else(|| Error::ReplayLogCorrupt {
                path: path.to_path_buf(),
                offset,
                reason: "timestamp exceeds system time range".to_string(),
            })
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

    fn append_chunk(log: &mut impl JobReplayLog, data: &[u8]) -> Result<Seq> {
        log.append(AppendLogEvent {
            stream: LogStream::Stdout,
            data: Bytes::copy_from_slice(data),
            timestamp: now(),
        })
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

    #[test]
    fn replay_log_append_assigns_offset_sequences() {
        let mut log = InMemoryJobReplayLog::new();
        assert_eq!(append_chunk(&mut log, b"a").unwrap(), 0);
        assert_eq!(append_chunk(&mut log, b"b").unwrap(), 1);

        let replay = log.replay_from(0);
        let seqs: Vec<Seq> = replay.events().iter().map(|ev| ev.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
        assert_eq!(log.next_seq(), 2);
    }

    #[test]
    fn replay_log_cursor_returns_suffix_and_empty_past_end() {
        let mut log = InMemoryJobReplayLog::new();
        for data in [b"a", b"b", b"c"] {
            append_chunk(&mut log, data).unwrap();
        }

        let replay = log.replay_from(1);
        let bytes: Vec<&[u8]> = replay.events().iter().map(|ev| ev.data.as_ref()).collect();
        assert_eq!(bytes, vec![b"b".as_slice(), b"c".as_slice()]);
        assert!(log.replay_from(99).is_empty());
    }

    #[test]
    fn replay_log_overflow_is_fail_closed() {
        let mut log = InMemoryJobReplayLog::with_cap(3);
        append_chunk(&mut log, b"ab").unwrap();

        let result = append_chunk(&mut log, b"cd");
        assert!(
            matches!(result, Err(Error::LogBufferOverflow { cap: 3 })),
            "expected LogBufferOverflow(cap=3), got {result:?}"
        );

        let replay = log.replay_from(0);
        assert_eq!(replay.events().len(), 1);
        assert_eq!(replay.events()[0].data.as_ref(), b"ab");
        assert_eq!(log.next_seq(), 1);
    }

    #[test]
    fn file_replay_log_reopens_without_renumbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        let first_timestamp = UNIX_EPOCH + Duration::new(42, 7);
        let second_timestamp = UNIX_EPOCH + Duration::new(43, 8);
        let third_timestamp = UNIX_EPOCH + Duration::new(44, 9);

        {
            let mut log = FileJobReplayLog::open(&path).unwrap();
            assert_eq!(
                log.append(AppendLogEvent {
                    stream: LogStream::Stdout,
                    data: Bytes::copy_from_slice(b"hello\0raw"),
                    timestamp: first_timestamp,
                })
                .unwrap(),
                0
            );
            assert_eq!(
                log.append(AppendLogEvent {
                    stream: LogStream::Stderr,
                    data: Bytes::copy_from_slice(b"stderr"),
                    timestamp: second_timestamp,
                })
                .unwrap(),
                1
            );
            assert_eq!(log.next_seq(), 2);
        }

        let mut reopened = FileJobReplayLog::open(&path).unwrap();
        let replay = reopened.replay_from(1);
        assert_eq!(replay.events().len(), 1);
        assert_eq!(replay.events()[0].seq, 1);
        assert_eq!(replay.events()[0].stream, LogStream::Stderr);
        assert_eq!(replay.events()[0].data.as_ref(), b"stderr");
        assert_eq!(replay.events()[0].timestamp, second_timestamp);
        assert_eq!(reopened.next_seq(), 2);

        assert_eq!(
            reopened
                .append(AppendLogEvent {
                    stream: LogStream::Stdout,
                    data: Bytes::copy_from_slice(b"after-reopen"),
                    timestamp: third_timestamp,
                })
                .unwrap(),
            2
        );

        let reopened_again = FileJobReplayLog::open(&path).unwrap();
        let replay = reopened_again.replay_from(0);
        let seqs: Vec<Seq> = replay.events().iter().map(|event| event.seq).collect();
        let bytes: Vec<&[u8]> = replay
            .events()
            .iter()
            .map(|event| event.data.as_ref())
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
        assert_eq!(
            bytes,
            vec![
                b"hello\0raw".as_slice(),
                b"stderr".as_slice(),
                b"after-reopen".as_slice()
            ]
        );
        assert_eq!(reopened_again.next_seq(), 3);
    }

    #[test]
    fn file_replay_log_preserves_pre_epoch_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        let timestamp = UNIX_EPOCH - Duration::new(2, 250);

        {
            let mut log = FileJobReplayLog::open(&path).unwrap();
            log.append(AppendLogEvent {
                stream: LogStream::Stdout,
                data: Bytes::copy_from_slice(b"before-epoch"),
                timestamp,
            })
            .unwrap();
        }

        let reopened = FileJobReplayLog::open(&path).unwrap();
        let replay = reopened.replay_from(0);
        assert_eq!(replay.events()[0].timestamp, timestamp);
    }

    #[test]
    fn file_replay_log_detects_truncated_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        std::fs::write(&path, FILE_LOG_MAGIC).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&0u64.to_le_bytes())
            .unwrap();

        let result = FileJobReplayLog::open(&path);
        assert!(
            matches!(result, Err(Error::ReplayLogCorrupt { .. })),
            "expected ReplayLogCorrupt for truncated frame, got {result:?}"
        );
    }

    #[test]
    fn file_replay_log_overflow_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        let mut log = FileJobReplayLog::with_cap(&path, 3).unwrap();
        append_chunk(&mut log, b"ab").unwrap();

        let result = append_chunk(&mut log, b"cd");
        assert!(
            matches!(result, Err(Error::LogBufferOverflow { cap: 3 })),
            "expected LogBufferOverflow(cap=3), got {result:?}"
        );
        assert_eq!(log.next_seq(), 1);
        drop(log);

        let reopened = FileJobReplayLog::with_cap(&path, 3).unwrap();
        let replay = reopened.replay_from(0);
        assert_eq!(replay.events().len(), 1);
        assert_eq!(replay.events()[0].seq, 0);
        assert_eq!(replay.events()[0].data.as_ref(), b"ab");
        assert_eq!(reopened.next_seq(), 1);
    }
}
