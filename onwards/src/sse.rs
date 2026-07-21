//! SSE (Server-Sent Events) stream buffering
//!
//! This module provides a stream wrapper that buffers incomplete SSE events.
//! Some AI providers send partial chunks that split JSON data across multiple
//! network packets. This buffer accumulates bytes until a complete SSE event
//! (terminated by `\n\n` or `\r\n\r\n`) is received before forwarding.

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

/// Error emitted while reassembling an SSE stream.
#[derive(Debug)]
pub enum SseStreamError<E> {
    /// The upstream byte stream failed.
    Upstream(E),
    /// A single SSE event exceeded the per-event buffer limit.
    EventTooLarge { max_size: usize },
}

impl<E: fmt::Display> fmt::Display for SseStreamError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream(error) => error.fmt(formatter),
            Self::EventTooLarge { max_size } => {
                write!(
                    formatter,
                    "SSE event exceeded maximum size of {max_size} bytes"
                )
            }
        }
    }
}

impl<E: Error + 'static> Error for SseStreamError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Upstream(error) => Some(error),
            Self::EventTooLarge { .. } => None,
        }
    }
}

/// A stream wrapper that buffers SSE events until they are complete.
///
/// SSE events are delimited by `\n\n` or `\r\n\r\n`. This wrapper accumulates
/// incoming bytes and only yields complete events, preventing consumers from
/// receiving partial JSON data.
///
/// The buffer is capped at [`MAX_SSE_BUFFER_SIZE`] bytes to prevent memory
/// exhaustion from malicious or buggy upstream providers.
pub struct SseBufferedStream<S> {
    inner: S,
    buffer: BytesMut,
    pending: Option<Bytes>,
    terminated: bool,
}

impl<S> SseBufferedStream<S> {
    /// Wrap an existing stream with SSE buffering.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
            pending: None,
            terminated: false,
        }
    }
}

impl<S, E> Stream for SseBufferedStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, SseStreamError<E>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        if this.terminated {
            return Poll::Ready(None);
        }

        loop {
            // Check if buffer contains a complete event.
            if let Some(end) = find_event_boundary(&this.buffer) {
                // Extract the complete event up to and including its delimiter.
                let complete = this.buffer.split_to(end);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            // Ingest only as much of the current transport chunk as fits in the
            // per-event buffer. Complete events are yielded before the remainder
            // of the transport chunk is consumed, so transport chunking cannot
            // make a sequence of small events look like one oversized event.
            if let Some(mut pending) = this.pending.take() {
                let remaining_capacity = MAX_SSE_BUFFER_SIZE - this.buffer.len();
                if remaining_capacity == 0 {
                    tracing::error!(
                        "SSE event exceeded maximum size of {} bytes, terminating stream",
                        MAX_SSE_BUFFER_SIZE
                    );
                    this.buffer.clear();
                    this.terminated = true;
                    return Poll::Ready(Some(Err(SseStreamError::EventTooLarge {
                        max_size: MAX_SSE_BUFFER_SIZE,
                    })));
                }

                let buffered_len = pending.len().min(remaining_capacity);
                this.buffer
                    .extend_from_slice(&pending.split_to(buffered_len));
                if !pending.is_empty() {
                    this.pending = Some(pending);
                }
                continue;
            }

            // Need more data - poll the inner stream
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if !chunk.is_empty() {
                        this.pending = Some(chunk);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(SseStreamError::Upstream(e))));
                }
                Poll::Ready(None) => {
                    // Stream ended - flush any remaining data
                    if this.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    // Return whatever is left (may be incomplete, but stream is done)
                    let remaining = this.buffer.split().freeze();
                    return Poll::Ready(Some(Ok(remaining)));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Find the end of the first LF- or CRLF-delimited event in the buffer.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| position + 2);
    let crlf = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4);

    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(lf.min(crlf)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
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
    async fn test_incomplete_event_at_stream_end() {
        // Stream ends without final \n\n
        let chunks = vec![b"data: incomplete".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: incomplete");
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
    async fn test_preserves_utf8_code_point_split_across_transport_chunks() {
        let event = "data: {\"content\":\"🙂\"}\n\n".as_bytes();
        let emoji_start = event
            .windows("🙂".len())
            .position(|window| window == "🙂".as_bytes())
            .unwrap();
        let chunks = vec![
            Bytes::copy_from_slice(&event[..emoji_start + 2]),
            Bytes::copy_from_slice(&event[emoji_start + 2..]),
        ];
        let inner = futures_util::stream::iter(chunks.into_iter().map(Ok::<_, Infallible>));
        let results: Vec<_> = SseBufferedStream::new(inner).collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), event);
    }

    #[tokio::test]
    async fn test_handles_crlf_events() {
        let chunks = vec![b"data: first\r\n\r\ndata: second\r\n\r\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: first\r\n\r\n"
        );
        assert_eq!(
            results[1].as_ref().unwrap().as_ref(),
            b"data: second\r\n\r\n"
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
    async fn test_oversized_event_returns_stream_error() {
        let mut large_event = vec![b'x'; MAX_SSE_BUFFER_SIZE - 1];
        large_event.extend_from_slice(b"\n\n");
        assert_eq!(large_event.len(), MAX_SSE_BUFFER_SIZE + 1);
        let chunks: Vec<&[u8]> = vec![&large_event];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        assert!(matches!(
            results.as_slice(),
            [Err(SseStreamError::EventTooLarge { max_size })]
                if *max_size == MAX_SSE_BUFFER_SIZE
        ));
    }

    #[tokio::test]
    async fn test_oversized_event_does_not_allocate_oversized_buffer() {
        let mut large_event = vec![b'x'; MAX_SSE_BUFFER_SIZE - 1];
        large_event.extend_from_slice(b"\n\n");
        let inner = futures_util::stream::iter([Ok::<_, Infallible>(Bytes::from(large_event))]);
        let mut stream = SseBufferedStream::new(inner);

        assert!(matches!(
            stream.next().await,
            Some(Err(SseStreamError::EventTooLarge { max_size }))
                if max_size == MAX_SSE_BUFFER_SIZE
        ));
        assert!(stream.buffer.capacity() <= MAX_SSE_BUFFER_SIZE);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_oversized_transport_chunk_with_small_events_is_processed() {
        const EVENT: &[u8] = b"data: ok\n\n";
        let event_count = MAX_SSE_BUFFER_SIZE / EVENT.len() + 2;
        let chunk = Bytes::from(EVENT.repeat(event_count));
        assert!(chunk.len() > MAX_SSE_BUFFER_SIZE);

        let inner = futures_util::stream::iter([Ok::<_, Infallible>(chunk)]);
        let stream = SseBufferedStream::new(inner);
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), event_count);
        assert!(
            results
                .iter()
                .all(|result| result.as_ref().is_ok_and(|event| event.as_ref() == EVENT))
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
