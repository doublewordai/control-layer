//! SSE (Server-Sent Events) stream buffering
//!
//! This module provides a stream wrapper that buffers incomplete SSE events.
//! Some AI providers send partial chunks that split JSON data and line endings
//! across network packets. This buffer accumulates bytes until a complete SSE
//! event is received before forwarding it unchanged.

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Maximum buffer size per SSE stream (64KB).
///
/// This prevents memory exhaustion from malicious or buggy providers that
/// send endless data without event delimiters. Typical SSE events are under
/// 1KB, so 64KB provides ample headroom while capping worst-case memory at
/// ~64MB for 1000 concurrent streams.
const MAX_SSE_BUFFER_SIZE: usize = 64 * 1024;

/// A framing failure detected while reassembling an SSE stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseFramingError {
    /// The source ended with bytes that did not form a complete SSE event.
    IncompleteEvent,
    /// A single event exceeded the bounded reassembly buffer.
    BufferOverflow { limit: usize },
}

impl fmt::Display for SseFramingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteEvent => {
                write!(f, "upstream SSE stream ended with an incomplete event")
            }
            Self::BufferOverflow { limit } => {
                write!(f, "upstream SSE event exceeded the {limit}-byte limit")
            }
        }
    }
}

impl Error for SseFramingError {}

/// An error from either the source body or SSE framing validation.
#[derive(Debug)]
pub enum SseStreamError<E> {
    Source(E),
    Framing(SseFramingError),
}

impl<E: fmt::Display> fmt::Display for SseStreamError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Framing(error) => error.fmt(f),
        }
    }
}

impl<E: Error + 'static> Error for SseStreamError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Framing(error) => Some(error),
        }
    }
}

#[derive(Default)]
struct EventBoundaryScanner {
    line_has_bytes: bool,
    pending_cr: bool,
}

enum ScanOutcome {
    Boundary(usize),
    NeedMore(usize),
    Limit,
}

impl EventBoundaryScanner {
    fn scan(&mut self, input: &[u8], remaining: usize) -> ScanOutcome {
        let mut consumed = 0;

        while consumed < input.len() {
            let byte = input[consumed];
            if self.pending_cr {
                if byte == b'\n' {
                    if consumed == remaining {
                        return ScanOutcome::Limit;
                    }
                    let blank_line = !self.line_has_bytes;
                    self.pending_cr = false;
                    self.line_has_bytes = false;
                    consumed += 1;
                    if blank_line {
                        return ScanOutcome::Boundary(consumed);
                    }
                    continue;
                }

                let blank_line = !self.line_has_bytes;
                self.pending_cr = false;
                self.line_has_bytes = false;
                if blank_line {
                    return ScanOutcome::Boundary(consumed);
                }
            }

            if consumed == remaining {
                return ScanOutcome::Limit;
            }

            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' => {
                    let blank_line = !self.line_has_bytes;
                    self.line_has_bytes = false;
                    consumed += 1;
                    if blank_line {
                        return ScanOutcome::Boundary(consumed);
                    }
                    continue;
                }
                _ => self.line_has_bytes = true,
            }
            consumed += 1;
        }

        ScanOutcome::NeedMore(consumed)
    }

    fn finish_eof(&mut self) -> bool {
        if !self.pending_cr {
            return false;
        }
        let blank_line = !self.line_has_bytes;
        self.pending_cr = false;
        self.line_has_bytes = false;
        blank_line
    }
}

/// Internal SSE framing that keeps source failures distinct from framing failures.
pub(crate) struct CheckedSseStream<S> {
    inner: S,
    max_buffer_size: usize,
    buffer: BytesMut,
    pending_chunk: Option<Bytes>,
    pending_offset: usize,
    scanner: EventBoundaryScanner,
    source_done: bool,
    terminated: bool,
    incomplete_bytes: Option<Bytes>,
    #[cfg(test)]
    scanned_bytes: usize,
}

impl<S> CheckedSseStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self::with_max_buffer_size(inner, MAX_SSE_BUFFER_SIZE)
    }

    pub(crate) fn with_max_buffer_size(inner: S, max_buffer_size: usize) -> Self {
        Self {
            inner,
            max_buffer_size,
            buffer: BytesMut::new(),
            pending_chunk: None,
            pending_offset: 0,
            scanner: EventBoundaryScanner::default(),
            source_done: false,
            terminated: false,
            incomplete_bytes: None,
            #[cfg(test)]
            scanned_bytes: 0,
        }
    }

    fn append_pending(&mut self) -> ScanOutcome {
        let chunk = self.pending_chunk.as_ref().expect("pending chunk checked");
        let input = &chunk[self.pending_offset..];
        let outcome = self.scanner.scan(
            input,
            self.max_buffer_size.saturating_sub(self.buffer.len()),
        );
        let consumed = match outcome {
            ScanOutcome::Boundary(consumed) | ScanOutcome::NeedMore(consumed) => consumed,
            ScanOutcome::Limit => 0,
        };

        if consumed > 0 {
            self.buffer.extend_from_slice(&input[..consumed]);
            self.pending_offset += consumed;
            #[cfg(test)]
            {
                self.scanned_bytes += consumed;
            }
        }
        if self.pending_offset == chunk.len() {
            self.pending_chunk = None;
            self.pending_offset = 0;
        }
        outcome
    }

    fn take_incomplete_bytes(&mut self) -> Option<Bytes> {
        self.incomplete_bytes.take()
    }

    #[cfg(test)]
    fn scanned_bytes_for_test(&self) -> usize {
        self.scanned_bytes
    }

    #[cfg(test)]
    fn buffer_capacity_for_test(&self) -> usize {
        self.buffer.capacity()
    }
}

impl<S, E> Stream for CheckedSseStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, SseStreamError<E>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            if this.terminated {
                return Poll::Ready(None);
            }

            if this.pending_chunk.is_some() {
                match this.append_pending() {
                    ScanOutcome::Boundary(_) => {
                        this.scanner = EventBoundaryScanner::default();
                        return Poll::Ready(Some(Ok(this.buffer.split().freeze())));
                    }
                    ScanOutcome::NeedMore(_) => continue,
                    ScanOutcome::Limit => {
                        this.buffer.clear();
                        this.pending_chunk = None;
                        this.terminated = true;
                        return Poll::Ready(Some(Err(SseStreamError::Framing(
                            SseFramingError::BufferOverflow {
                                limit: this.max_buffer_size,
                            },
                        ))));
                    }
                }
            }

            if this.source_done {
                if this.buffer.is_empty() {
                    this.terminated = true;
                    return Poll::Ready(None);
                }
                if this.scanner.finish_eof() {
                    this.scanner = EventBoundaryScanner::default();
                    return Poll::Ready(Some(Ok(this.buffer.split().freeze())));
                }
                this.incomplete_bytes = Some(this.buffer.split().freeze());
                this.terminated = true;
                return Poll::Ready(Some(Err(SseStreamError::Framing(
                    SseFramingError::IncompleteEvent,
                ))));
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if !chunk.is_empty() {
                        this.pending_chunk = Some(chunk);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    this.buffer.clear();
                    this.terminated = true;
                    return Poll::Ready(Some(Err(SseStreamError::Source(e))));
                }
                Poll::Ready(None) => {
                    this.source_done = true;
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// A stream wrapper that buffers SSE events until they are complete.
///
/// This public wrapper preserves its original source error type. Internal proxy
/// paths use checked framing when incomplete or oversized events must be
/// distinguishable from source EOF.
pub struct SseBufferedStream<S> {
    checked: CheckedSseStream<S>,
}

impl<S> SseBufferedStream<S> {
    /// Wrap an existing stream with SSE buffering.
    pub fn new(inner: S) -> Self {
        Self {
            checked: CheckedSseStream::new(inner),
        }
    }
}

impl<S, E> Stream for SseBufferedStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.checked).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(Err(SseStreamError::Source(error)))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(Some(Err(SseStreamError::Framing(SseFramingError::IncompleteEvent)))) => {
                Poll::Ready(self.checked.take_incomplete_bytes().map(Ok))
            }
            Poll::Ready(Some(Err(SseStreamError::Framing(SseFramingError::BufferOverflow {
                ..
            })))) => Poll::Ready(None),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(crate) fn framing_error_in_chain(error: &(dyn Error + 'static)) -> Option<SseFramingError> {
    let mut current = Some(error);
    while let Some(source) = current {
        if let Some(framing) = source.downcast_ref::<SseFramingError>() {
            return Some(*framing);
        }
        current = source.source();
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedSseEvent {
    Comment,
    Data {
        data: Vec<u8>,
        event_type: Option<Vec<u8>>,
    },
    Invalid,
}

/// Parse one framed event without normalizing the bytes forwarded downstream.
pub(crate) fn parse_sse_event(event: &[u8]) -> ParsedSseEvent {
    let mut position = if event.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let mut data = Vec::new();
    let mut found_data = false;
    let mut event_type = None;

    while position < event.len() {
        let line_start = position;
        while position < event.len() && !matches!(event[position], b'\r' | b'\n') {
            position += 1;
        }
        let line = &event[line_start..position];

        if position < event.len() {
            if event[position] == b'\r'
                && position + 1 < event.len()
                && event[position + 1] == b'\n'
            {
                position += 2;
            } else {
                position += 1;
            }
        }

        if line.is_empty() || line.starts_with(b":") {
            continue;
        }

        if line == b"event" {
            if event_type.replace(Vec::new()).is_some() {
                return ParsedSseEvent::Invalid;
            }
            continue;
        }
        if let Some(value) = line.strip_prefix(b"event:") {
            if event_type
                .replace(value.strip_prefix(b" ").unwrap_or(value).to_vec())
                .is_some()
            {
                return ParsedSseEvent::Invalid;
            }
            continue;
        }

        let value = if line == b"data" {
            &b""[..]
        } else if let Some(value) = line.strip_prefix(b"data:") {
            value.strip_prefix(b" ").unwrap_or(value)
        } else {
            return ParsedSseEvent::Invalid;
        };
        if found_data {
            data.push(b'\n');
        }
        found_data = true;
        data.extend_from_slice(value);
    }

    if found_data {
        ParsedSseEvent::Data { data, event_type }
    } else if event_type.is_some() {
        ParsedSseEvent::Invalid
    } else {
        ParsedSseEvent::Comment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::convert::Infallible;

    /// Helper to create a stream from chunks
    fn chunks_to_stream(
        chunks: Vec<&'static [u8]>,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> + Unpin {
        futures_util::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[test]
    fn public_buffered_stream_preserves_source_error_item_type() {
        fn assert_item_type<S, E>(_stream: &SseBufferedStream<S>)
        where
            S: Stream<Item = Result<Bytes, E>> + Unpin,
            SseBufferedStream<S>: Stream<Item = Result<Bytes, E>>,
        {
        }

        let stream = SseBufferedStream::new(chunks_to_stream(Vec::new()));
        assert_item_type::<_, Infallible>(&stream);
    }

    #[tokio::test]
    async fn checked_stream_handles_highly_fragmented_input_in_linear_state() {
        let expected = Bytes::from_static(b"data: fragmented\r\n\r\n");
        let chunks = expected
            .iter()
            .copied()
            .map(|byte| Ok::<_, Infallible>(Bytes::from(vec![byte])));
        let mut stream = CheckedSseStream::new(futures_util::stream::iter(chunks));

        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event, expected);
        assert_eq!(stream.scanned_bytes_for_test(), expected.len());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn checked_stream_does_not_copy_a_huge_source_chunk() {
        let huge = Bytes::from(vec![b'x'; MAX_SSE_BUFFER_SIZE * 8]);
        let mut stream =
            CheckedSseStream::new(futures_util::stream::iter([Ok::<_, Infallible>(huge)]));

        assert!(matches!(
            stream.next().await,
            Some(Err(SseStreamError::Framing(
                SseFramingError::BufferOverflow {
                    limit: MAX_SSE_BUFFER_SIZE
                }
            )))
        ));
        assert!(stream.buffer_capacity_for_test() <= MAX_SSE_BUFFER_SIZE);
    }

    #[tokio::test]
    async fn checked_stream_copies_only_through_a_boundary_in_a_huge_chunk() {
        let mut source = b"data: first\n\n".to_vec();
        source.extend(std::iter::repeat_n(b'x', MAX_SSE_BUFFER_SIZE * 8));
        let mut stream = CheckedSseStream::new(futures_util::stream::iter([Ok::<_, Infallible>(
            Bytes::from(source),
        )]));

        assert_eq!(
            stream.next().await.unwrap().unwrap().as_ref(),
            b"data: first\n\n"
        );
        assert!(stream.buffer_capacity_for_test() < MAX_SSE_BUFFER_SIZE);
        assert!(matches!(
            stream.next().await,
            Some(Err(SseStreamError::Framing(
                SseFramingError::BufferOverflow { .. }
            )))
        ));
    }

    #[tokio::test]
    async fn test_complete_event_passes_through() {
        let chunks = vec![b"data: {\"hello\": \"world\"}\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"hello\": \"world\"}\n\n"
        );
    }

    #[tokio::test]
    async fn test_split_event_is_buffered() {
        // Event split across two chunks
        let chunks = vec![
            b"data: {\"hel".as_slice(),
            b"lo\": \"world\"}\n\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"hello\": \"world\"}\n\n"
        );
    }

    #[tokio::test]
    async fn test_multiple_events_in_one_chunk() {
        let chunks = vec![b"data: first\n\ndata: second\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: first\n\n");
        assert_eq!(results[1].as_ref().unwrap().as_ref(), b"data: second\n\n");
    }

    #[tokio::test]
    async fn test_event_split_at_newline() {
        // Split right at the delimiter
        let chunks = vec![b"data: test\n".as_slice(), b"\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: test\n\n");
    }

    #[tokio::test]
    async fn test_multiple_events_across_chunks() {
        let chunks = vec![
            b"data: first\n\ndata: sec".as_slice(),
            b"ond\n\ndata: third\n\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: first\n\n");
        assert_eq!(results[1].as_ref().unwrap().as_ref(), b"data: second\n\n");
        assert_eq!(results[2].as_ref().unwrap().as_ref(), b"data: third\n\n");
    }

    #[tokio::test]
    async fn test_incomplete_event_at_stream_end_is_a_framing_error() {
        let chunks = vec![b"data: incomplete".as_slice()];
        let stream = CheckedSseStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(SseStreamError::Framing(SseFramingError::IncompleteEvent))
        ));
    }

    #[tokio::test]
    async fn test_empty_stream() {
        let chunks: Vec<&[u8]> = vec![];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    async fn test_json_split_across_many_chunks() {
        // Simulate very fragmented delivery
        let chunks = vec![
            b"da".as_slice(),
            b"ta: ".as_slice(),
            b"{\"delta\"".as_slice(),
            b": {\"".as_slice(),
            b"content\": \"Hello".as_slice(),
            b"\"}}\n".as_slice(),
            b"\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"delta\": {\"content\": \"Hello\"}}\n\n"
        );
    }

    #[tokio::test]
    async fn test_fragmented_lf_crlf_cr_and_mixed_boundaries_preserve_exact_bytes() {
        for (chunks, expected) in [
            (
                vec![b"data: lf\n".as_slice(), b"\n".as_slice()],
                b"data: lf\n\n".as_slice(),
            ),
            (
                vec![
                    b"data: crlf\r".as_slice(),
                    b"\n\r".as_slice(),
                    b"\n".as_slice(),
                ],
                b"data: crlf\r\n\r\n".as_slice(),
            ),
            (
                vec![b"data: cr\r".as_slice(), b"\r".as_slice()],
                b"data: cr\r\r".as_slice(),
            ),
            (
                vec![b"data: mixed\r\n\r".as_slice(), b"\n".as_slice()],
                b"data: mixed\r\n\r\n".as_slice(),
            ),
        ] {
            let stream = SseBufferedStream::new(chunks_to_stream(chunks));
            let results: Vec<_> = stream.collect().await;

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].as_ref().unwrap().as_ref(), expected);
        }
    }

    #[tokio::test]
    async fn test_crlf_event_is_available_before_source_eof() {
        let source = futures_util::stream::once(async {
            Ok::<_, Infallible>(Bytes::from_static(b"data: ready\r\n\r\n"))
        })
        .chain(futures_util::stream::pending());
        let mut stream = SseBufferedStream::new(Box::pin(source));

        let event = tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
            .await
            .expect("complete CRLF event should not wait for EOF")
            .expect("event")
            .expect("valid frame");

        assert_eq!(event.as_ref(), b"data: ready\r\n\r\n");
    }

    #[tokio::test]
    async fn test_utf8_bom_is_preserved_at_stream_start() {
        let chunks = vec![
            b"\xef".as_slice(),
            b"\xbb\xbfdata:value\r".as_slice(),
            b"\r".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"\xef\xbb\xbfdata:value\r\r"
        );
    }

    #[tokio::test]
    async fn test_preserves_multiline_data() {
        // SSE can have multi-line data fields
        let chunks = vec![b"data: line1\ndata: line2\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: line1\ndata: line2\n\n"
        );
    }

    #[tokio::test]
    async fn test_buffer_overflow_is_a_framing_error() {
        // Create a chunk larger than MAX_SSE_BUFFER_SIZE without \n\n
        let large_chunk = vec![b'x'; MAX_SSE_BUFFER_SIZE + 1];
        let chunks: Vec<&[u8]> = vec![&large_chunk];
        let stream = CheckedSseStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(SseStreamError::Framing(SseFramingError::BufferOverflow {
                limit: MAX_SSE_BUFFER_SIZE
            }))
        ));
    }

    #[tokio::test]
    async fn test_source_body_error_remains_distinct_from_framing_errors() {
        let source = futures_util::stream::iter(vec![Err::<Bytes, _>(std::io::Error::other(
            "source failed",
        ))]);
        let results: Vec<_> = CheckedSseStream::new(source).collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(SseStreamError::Source(_))));
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "source failed"
        );
    }

    #[tokio::test]
    async fn test_buffer_at_limit_still_works() {
        // Create a chunk exactly at MAX_SSE_BUFFER_SIZE with \n\n at the end
        let mut chunk = vec![b'x'; MAX_SSE_BUFFER_SIZE - 2];
        chunk.extend_from_slice(b"\n\n");
        let chunks: Vec<&[u8]> = vec![&chunk];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
        assert_eq!(results[0].as_ref().unwrap().len(), MAX_SSE_BUFFER_SIZE);
    }
}
