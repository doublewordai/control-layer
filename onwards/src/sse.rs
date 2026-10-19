//! SSE (Server-Sent Events) stream buffering
//!
//! This module provides a stream wrapper that buffers incomplete SSE events.
//! Some AI providers send partial chunks that split JSON data across multiple
//! network packets. This buffer accumulates bytes until a complete SSE event
//! (terminated by `\n\n`) is received before forwarding.

use std::error::Error;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::Stream;

/// Default maximum pending bytes of an unfinished SSE event (64 KiB).
///
/// Complete events are forwarded before this limit is checked. Increase via
/// [`SseBufferedStream::with_limit`] for providers with large fragmented events.
pub const DEFAULT_SSE_BUFFER_LIMIT: usize = 64 * 1024;

/// Buffers SSE events until their `\n\n` delimiter arrives.
///
/// The limit applies only to unfinished data, not the total response or a
/// transport chunk containing many complete events. Exceeding it yields a body
/// error and drops the upstream stream; it must never look like a clean EOF.
pub struct SseBufferedStream<S> {
    inner: Option<S>,
    buffer: BytesMut,
    limit: usize,
}

impl<S> SseBufferedStream<S> {
    /// Wrap a stream with the default unfinished-event limit.
    pub fn new(inner: S) -> Self {
        Self::with_limit(inner, DEFAULT_SSE_BUFFER_LIMIT)
    }

    /// Wrap a stream with a limit on pending bytes of an unfinished event.
    /// Zero permits no unfinished bytes; it does not disable the limit.
    pub fn with_limit(inner: S, limit: usize) -> Self {
        Self {
            inner: Some(inner),
            buffer: BytesMut::new(),
            limit,
        }
    }
}

impl<S, E> Stream for SseBufferedStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<Box<dyn Error + Send + Sync>>,
{
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            // Drain complete events before limiting the unfinished remainder.
            // A network read may coalesce arbitrarily many valid events.
            if let Some(pos) = find_event_boundary(&this.buffer) {
                let complete = this.buffer.split_to(pos + 2);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            if this.buffer.len() > this.limit {
                tracing::error!(
                    buffered_bytes = this.buffer.len(),
                    limit_bytes = this.limit,
                    "Unfinished SSE event exceeded buffer limit"
                );
                this.buffer = BytesMut::new();
                this.inner = None;
                return Poll::Ready(Some(Err(axum::Error::new(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Unfinished SSE event exceeded buffer limit of {} bytes",
                        this.limit
                    ),
                )))));
            }

            let Some(inner) = this.inner.as_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.buffer = BytesMut::new();
                    this.inner = None;
                    return Poll::Ready(Some(Err(axum::Error::new(e))));
                }
                Poll::Ready(None) => {
                    this.inner = None;
                    if this.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    // Preserve the existing EOF behavior for a partial final event.
                    let remaining = this.buffer.split().freeze();
                    return Poll::Ready(Some(Ok(remaining)));
                }
                Poll::Pending => return Poll::Pending,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
    async fn test_coalesced_events_above_buffer_limit_are_preserved() {
        let event = b"data: small event\n\n";
        let chunk = event.repeat(10_000);
        let stream = SseBufferedStream::new(futures_util::stream::iter([Ok::<_, Infallible>(
            Bytes::from(chunk.clone()),
        )]));
        let results: Vec<_> = stream.collect().await;
        assert_eq!(results.len(), 10_000);
        let forwarded: Vec<u8> = results.into_iter().flat_map(|r| r.unwrap()).collect();
        assert_eq!(forwarded, chunk);
    }

    #[tokio::test]
    async fn test_buffer_overflow_returns_error() {
        // Create a chunk larger than DEFAULT_SSE_BUFFER_LIMIT without \n\n
        let large_chunk = vec![b'x'; DEFAULT_SSE_BUFFER_LIMIT + 1];
        let chunks: Vec<&[u8]> = vec![&large_chunk];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        // The body must fail explicitly, never report a clean EOF.
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("buffer limit")
        );
    }

    #[tokio::test]
    async fn test_buffer_at_limit_still_works() {
        // Create a chunk exactly at DEFAULT_SSE_BUFFER_LIMIT with \n\n at the end
        let mut chunk = vec![b'x'; DEFAULT_SSE_BUFFER_LIMIT - 2];
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
        assert_eq!(results[0].as_ref().unwrap().len(), DEFAULT_SSE_BUFFER_LIMIT);
    }

    #[tokio::test]
    async fn test_configured_limit_allows_large_fragmented_tool_event() {
        let event = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"function\":{{\"arguments\":\"{}\"}}}}]}}}}]}}\n\n",
            "x".repeat(128 * 1024)
        );
        let chunks: Vec<_> = event
            .as_bytes()
            .chunks(16 * 1024)
            .map(|chunk| Ok::<_, Infallible>(Bytes::copy_from_slice(chunk)))
            .collect();
        let mut stream =
            SseBufferedStream::with_limit(futures_util::stream::iter(chunks), 256 * 1024);
        assert_eq!(
            stream.next().await.unwrap().unwrap().as_ref(),
            event.as_bytes()
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_complete_events_forwarded_before_oversized_remainder_errors() {
        let mut chunk = b"data: valid\n\n".to_vec();
        chunk.extend_from_slice(&[b'x'; 33]);
        let mut stream = SseBufferedStream::with_limit(
            futures_util::stream::iter([
                Ok::<_, Infallible>(Bytes::from(chunk)),
                Ok(Bytes::from_static(b"\n\ndata: must not resume\n\n")),
            ]),
            32,
        );
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: valid\n\n");
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_overflow_drops_upstream_and_errors_http_body() {
        struct Upstream(Arc<AtomicBool>);
        impl Stream for Upstream {
            type Item = Result<Bytes, Infallible>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Ready(Some(Ok(Bytes::from_static(b"unterminated"))))
            }
        }
        impl Drop for Upstream {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = SseBufferedStream::with_limit(Upstream(dropped.clone()), 8);
        let body = axum::body::Body::from_stream(stream);
        let error = axum::body::to_bytes(body, usize::MAX).await.unwrap_err();
        assert!(error.to_string().contains("buffer limit"));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_upstream_error_is_preserved_and_terminal() {
        let mut stream = SseBufferedStream::new(futures_util::stream::iter([
            Ok(Bytes::from_static(b"data: unfinished")),
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "upstream reset",
            )),
            Ok(Bytes::from_static(b"must not resume\n\n")),
        ]));
        assert!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("upstream reset")
        );
        assert!(stream.next().await.is_none());
    }
}
