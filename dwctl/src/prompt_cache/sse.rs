//! SSE (Server-Sent Events) stream buffering.
//!
//! A stream wrapper that buffers incomplete SSE events — some providers split a
//! single event's JSON across network packets, so we accumulate bytes until a
//! complete event (terminated by `\n\n`) before yielding. The cache injection
//! ([`super::inject`]) needs complete events to find + edit the terminal usage frame.
//!
//! Copied from onwards' generic `sse.rs` (which the core proxy also uses, so it can't
//! be relocated) — kept self-contained here for the dwctl-owned cache layer (§0).

use bytes::{Bytes, BytesMut};
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Maximum buffer size per SSE stream (64KB) — caps worst-case memory from a buggy or
/// malicious upstream that never emits an event delimiter.
const MAX_SSE_BUFFER_SIZE: usize = 64 * 1024;

/// A stream wrapper that buffers SSE events until they are complete (delimited by
/// `\n\n`), so consumers never see partial JSON.
pub struct SseBufferedStream<S> {
    inner: S,
    buffer: BytesMut,
}

impl<S> SseBufferedStream<S> {
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
            if let Some(pos) = find_event_boundary(&this.buffer) {
                let complete = this.buffer.split_to(pos + 2);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                    if this.buffer.len() > MAX_SSE_BUFFER_SIZE {
                        tracing::error!(
                            "SSE buffer exceeded maximum size of {} bytes, terminating stream",
                            MAX_SSE_BUFFER_SIZE
                        );
                        this.buffer.clear();
                        return Poll::Ready(None);
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
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

/// Find `\n\n` in the buffer, returning the index of the first `\n`.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|window| window == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::convert::Infallible;

    fn stream(chunks: Vec<&'static [u8]>) -> impl Stream<Item = Result<Bytes, Infallible>> + Unpin {
        futures::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn complete_event_passes_through() {
        let out: Vec<_> = SseBufferedStream::new(stream(vec![b"data: {\"a\":1}\n\n"])).collect().await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().as_ref(), b"data: {\"a\":1}\n\n");
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
}
