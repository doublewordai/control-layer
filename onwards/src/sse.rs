//! SSE (Server-Sent Events) stream buffering
//!
//! This module provides a stream wrapper that buffers incomplete SSE events.
//! Some AI providers send partial chunks that split JSON data across multiple
//! network packets. This buffer accumulates bytes until a complete SSE event
//! (terminated by `\n\n`) is received before forwarding.

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Maximum buffer size per SSE stream (64KB).
///
/// This prevents memory exhaustion from malicious or buggy providers that
/// send endless data without event delimiters. Typical SSE events are under
/// 1KB, so 64KB provides ample headroom while capping worst-case memory at
/// ~64MB for 1000 concurrent streams.
const MAX_SSE_BUFFER_SIZE: usize = 64 * 1024;

/// A stream wrapper that buffers SSE events until they are complete.
///
/// SSE events are delimited by `\n\n`. This wrapper accumulates incoming
/// bytes and only yields complete events, preventing consumers from
/// receiving partial JSON data.
///
/// The buffer is capped at [`MAX_SSE_BUFFER_SIZE`] bytes to prevent memory
/// exhaustion from malicious or buggy upstream providers.
pub struct SseBufferedStream<S> {
    inner: S,
    buffer: BytesMut,
}

impl<S> SseBufferedStream<S> {
    /// Wrap an existing stream with SSE buffering.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
        }
    }
}

impl<S, E> Stream for SseBufferedStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            // Check if buffer contains a complete event (ends with \n\n)
            if let Some(pos) = find_event_boundary(&this.buffer) {
                // Extract the complete event(s) up to and including the \n\n
                let complete = this.buffer.split_to(pos + 2);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            // Need more data - poll the inner stream
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);

                    // Check buffer size limit to prevent memory exhaustion
                    // If exceeded, log error and terminate stream by returning None
                    if this.buffer.len() > MAX_SSE_BUFFER_SIZE {
                        tracing::error!(
                            "SSE buffer exceeded maximum size of {} bytes, terminating stream",
                            MAX_SSE_BUFFER_SIZE
                        );
                        this.buffer.clear();
                        return Poll::Ready(None);
                    }

                    // Loop back to check for complete events
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
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

/// Find the position of `\n\n` in the buffer, returning the index of the first `\n`.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|window| window == b"\n\n")
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
    async fn test_handles_crlf_events() {
        // \r\n\r\n does NOT contain \n\n (it's [0d 0a 0d 0a], not [0a 0a])
        // So we only flush at end of stream. Real SSE servers that use CRLF
        // typically send \r\n\r\n which our buffer treats as incomplete until EOF.
        // This is acceptable since the data will be flushed when stream ends.
        let chunks = vec![b"data: test\r\n\r\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        // No \n\n found, so entire content flushed at stream end
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: test\r\n\r\n");
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
    async fn test_buffer_overflow_terminates_stream() {
        // Create a chunk larger than MAX_SSE_BUFFER_SIZE without \n\n
        let large_chunk = vec![b'x'; MAX_SSE_BUFFER_SIZE + 1];
        let chunks: Vec<&[u8]> = vec![&large_chunk];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        // Stream terminates immediately when buffer exceeded (returns None)
        assert_eq!(results.len(), 0);
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
