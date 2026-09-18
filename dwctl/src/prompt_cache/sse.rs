//! SSE (Server-Sent Events) stream buffering.
//!
//! A stream wrapper that buffers incomplete SSE events — some providers split a
//! single event's JSON across network packets, so we accumulate bytes until a
//! complete event (terminated by `\n\n`) before yielding. The cache injection
//! ([`super::inject`]) needs complete events to find + edit the terminal usage frame.
//!
//! Uses the same configured unfinished-event limit as the core proxy. Complete events
//! are forwarded before checking the pending remainder. An over-limit event produces
//! a stream error and drops the upstream, so the cache commit gate sees a failure.

use bytes::{Bytes, BytesMut};
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A stream wrapper that buffers SSE events until they are complete (delimited by
/// `\n\n`), so consumers never see partial JSON.
pub struct SseBufferedStream<S> {
    inner: Option<S>,
    buffer: BytesMut,
    limit: usize,
}

impl<S> SseBufferedStream<S> {
    pub fn new(inner: S) -> Self {
        Self::with_limit(inner, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT)
    }

    /// Limit pending bytes of an unfinished event; complete events are forwarded first.
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
    E: From<std::io::Error>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            if let Some(pos) = find_event_boundary(&this.buffer) {
                let complete = this.buffer.split_to(pos + 2);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            if this.buffer.len() > this.limit {
                tracing::error!(
                    buffered_bytes = this.buffer.len(),
                    limit_bytes = this.limit,
                    "Unfinished cache SSE event exceeded buffer limit"
                );
                this.buffer = BytesMut::new();
                this.inner = None;
                return Poll::Ready(Some(Err(E::from(std::io::Error::other(format!(
                    "Unfinished cache SSE event exceeded buffer limit of {} bytes",
                    this.limit
                ))))));
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
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    this.inner = None;
                    if this.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    let remaining = this.buffer.split().freeze();
                    return Poll::Ready(Some(Ok(remaining)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Find the end-of-event separator, returning `pos` such that `pos + 2` is the index just
/// past the separator (the caller's `split_to(pos + 2)` then yields the event with its
/// trailing blank line). Handles both `\n\n` (LF) and `\r\n\r\n` (CRLF), since the SSE spec
/// permits either; the separator that starts earlier wins.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        // `\r\n\r\n` is 4 bytes, so its end-marker is start + 2 to satisfy the `pos + 2` caller.
        (Some(l), Some(c)) => Some(if l <= c { l } else { c + 2 }),
        (Some(l), None) => Some(l),
        (None, Some(c)) => Some(c + 2),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    // Error type is io::Error to satisfy the `E: From<io::Error>` bound; these streams never err.
    fn stream(chunks: Vec<&'static [u8]>) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
        futures::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn complete_event_passes_through() {
        let out: Vec<_> = SseBufferedStream::new(stream(vec![b"data: {\"a\":1}\n\n"])).collect().await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().as_ref(), b"data: {\"a\":1}\n\n");
    }

    #[tokio::test]
    async fn crlf_separated_events_are_split() {
        // SSE permits CRLF (`\r\n\r\n`) separators — two events must come out as two items,
        // not accumulate until the buffer cap and error.
        let out: Vec<_> = SseBufferedStream::new(stream(vec![b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\n"]))
            .collect()
            .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().unwrap().as_ref(), b"data: {\"a\":1}\r\n\r\n");
        assert_eq!(out[1].as_ref().unwrap().as_ref(), b"data: {\"b\":2}\r\n\r\n");
    }

    #[tokio::test]
    async fn event_split_across_chunks_is_reassembled() {
        // One event split mid-JSON across two body chunks → yielded as one complete event.
        let out: Vec<_> = SseBufferedStream::new(stream(vec![b"data: {\"a\":", b"1}\n\n"]))
            .map(|r| r.unwrap())
            .collect()
            .await;
        let joined: Vec<u8> = out.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(joined, b"data: {\"a\":1}\n\n");
    }

    #[tokio::test]
    async fn trailing_incomplete_data_is_flushed_on_end() {
        let out: Vec<_> = SseBufferedStream::new(stream(vec![b"data: partial"]))
            .map(|r| r.unwrap())
            .collect()
            .await;
        let joined: Vec<u8> = out.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(joined, b"data: partial");
    }

    #[tokio::test]
    async fn over_limit_buffer_yields_error_not_clean_eof() {
        // An upstream that never emits a `\n\n` delimiter pushes the buffer past the cap.
        // The last item must be an Err (protocol violation surfaced), not a silent end.
        let big = vec![b'x'; onwards::sse::DEFAULT_SSE_BUFFER_LIMIT + 1];
        let s = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(big))]);
        let out: Vec<_> = SseBufferedStream::new(s).collect().await;
        assert_eq!(out.len(), 1, "one error item, then terminate");
        assert!(out[0].is_err(), "over-limit buffer surfaces an error, not a clean EOF");
    }

    #[tokio::test]
    async fn coalesced_complete_events_are_not_limited_as_one_buffer() {
        let event = b"data: {\"a\":1}\n\n";
        let data = event.repeat(10_000);
        assert!(data.len() > onwards::sse::DEFAULT_SSE_BUFFER_LIMIT);
        let input = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(data))]);
        let out: Vec<_> = SseBufferedStream::new(input).collect().await;
        assert_eq!(out.len(), 10_000);
        assert!(out.iter().all(|x| x.as_ref().unwrap().as_ref() == event));
    }

    #[tokio::test]
    async fn configured_limit_accepts_large_fragmented_event() {
        let data = format!("data: {{\"arguments\":\"{}\"}}\n\n", "x".repeat(512 * 1024));
        let chunks: Vec<_> = data
            .as_bytes()
            .chunks(8192)
            .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
            .collect();
        let input = futures::stream::iter(chunks);
        let out: Vec<_> = SseBufferedStream::with_limit(input, 1024 * 1024).collect().await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().as_ref(), data.as_bytes());
    }

    #[tokio::test]
    async fn overflow_drops_upstream_and_is_terminal() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct TrackedStream(Arc<AtomicBool>);
        impl Drop for TrackedStream {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        impl Stream for TrackedStream {
            type Item = Result<Bytes, std::io::Error>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Ready(Some(Ok(Bytes::from_static(b"unfinished"))))
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let mut stream = SseBufferedStream::with_limit(TrackedStream(dropped.clone()), 8);
        assert!(stream.next().await.unwrap().is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(stream.next().await.is_none());
    }
}
