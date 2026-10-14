//! How a sanitized stream ends when its upstream stops early.
//!
//! Once a streamed response has started, its HTTP status is already sent, so
//! the stream itself has to say how it ended. An upstream can stop partway: the
//! connection drops, an event is malformed or too large, or the body simply
//! ends without `[DONE]`. Aborting the response would leave the client with a
//! transport error, and ending it quietly would look like a complete answer.
//! Instead the stream ends with [`interrupted_event`], an error event in the
//! same shape as an upstream's embedded error, which official SDKs raise as an
//! API error and the batch reassembler reclassifies as a retryable status.
//!
//! When the generation had already finished and only the trailer was lost, the
//! stream ends with `[DONE]` instead.

use std::io;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::json;
use tracing::{debug, error, warn};

/// HTTP status carried in the interruption event: the upstream failed after
/// the response had started.
pub const INTERRUPTED_STATUS: u16 = 502;

/// The SSE event that ends a stream whose upstream stopped before finishing.
pub fn interrupted_event() -> Bytes {
    let envelope = json!({
        "error": {
            "message": "The response stream was interrupted before it finished. Retry the request.",
            "type": "api_error",
            "param": null,
            "code": INTERRUPTED_STATUS,
        }
    });
    Bytes::from(format!("data: {envelope}\n\n"))
}

const DONE_EVENT: &[u8] = b"data: [DONE]\n\n";

/// What a sanitized stream has forwarded so far.
#[derive(Debug, Default)]
pub(crate) struct StreamProgress {
    /// A terminal event was forwarded: `[DONE]`, an error, or a final
    /// Responses event.
    ended: bool,
    /// Each choice index seen, and whether it has a `finish_reason`.
    choices: Vec<(u32, bool)>,
    saw_usage: bool,
}

impl StreamProgress {
    pub(crate) fn record_end(&mut self) {
        self.ended = true;
    }

    pub(crate) fn record_choice(&mut self, index: u32, finished: bool) {
        match self.choices.iter_mut().find(|(i, _)| *i == index) {
            Some((_, done)) => *done |= finished,
            None => self.choices.push((index, finished)),
        }
    }

    pub(crate) fn record_usage(&mut self) {
        self.saw_usage = true;
    }

    /// Every choice has finished, or the final usage was reported.
    fn generation_finished(&self) -> bool {
        self.saw_usage || (!self.choices.is_empty() && self.choices.iter().all(|(_, done)| *done))
    }

    /// The event that closes a stream whose upstream stopped here, if any.
    fn closing_event(&self) -> Option<Bytes> {
        if self.ended {
            None
        } else if self.generation_finished() {
            Some(Bytes::from_static(DONE_EVENT))
        } else {
            Some(interrupted_event())
        }
    }
}

/// Run each upstream event through `sanitize`, and end the stream explicitly
/// if the upstream fails or stops before a terminal event.
///
/// `sanitize` returns an error for an event it cannot forward; that ends the
/// stream the same way as an upstream failure. The stream never yields an
/// error, so the client always receives a complete response body.
pub(crate) fn end_explicitly<S, E, F>(
    upstream: S,
    mut sanitize: F,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnMut(Bytes, &mut StreamProgress) -> Result<Bytes, io::Error> + Send + 'static,
{
    async_stream::stream! {
        let mut upstream = upstream;
        let mut progress = StreamProgress::default();
        while let Some(item) = upstream.next().await {
            let sanitized = match item {
                Ok(chunk) => sanitize(chunk, &mut progress),
                Err(e) => {
                    error!(error = %e, "Stream error");
                    Err(io::Error::other("upstream stream error"))
                }
            };
            match sanitized {
                Ok(bytes) => yield Ok(bytes),
                Err(_) => {
                    if let Some(event) = progress.closing_event() {
                        yield Ok(event);
                    }
                    return;
                }
            }
        }
        if let Some(event) = progress.closing_event() {
            if progress.generation_finished() {
                debug!("Upstream stream finished without [DONE]; adding it");
            } else {
                warn!("Upstream stream ended before it finished; ending it with an error event");
            }
            yield Ok(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    async fn collect(chunks: Vec<Result<&'static str, &'static str>>) -> Vec<String> {
        let upstream = stream::iter(
            chunks
                .into_iter()
                .map(|c| c.map(|s| Bytes::from_static(s.as_bytes())))
                .collect::<Vec<_>>(),
        );
        let out = end_explicitly(upstream, |chunk, progress| {
            match std::str::from_utf8(&chunk).unwrap() {
                "done" => progress.record_end(),
                "finish" => progress.record_choice(0, true),
                "content" => progress.record_choice(0, false),
                "bad" => return Err(io::Error::other("bad")),
                _ => {}
            }
            Ok(chunk)
        });
        out.map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await
    }

    fn interrupted() -> String {
        String::from_utf8(interrupted_event().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_complete_stream_is_unchanged() {
        assert_eq!(
            collect(vec![Ok("content"), Ok("finish"), Ok("done")]).await,
            ["content", "finish", "done"]
        );
    }

    #[tokio::test]
    async fn an_upstream_failure_mid_answer_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content"), Err("reset")]).await,
            vec!["content".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn an_early_end_mid_answer_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content")]).await,
            vec!["content".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn an_unforwardable_event_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content"), Ok("bad"), Ok("content")]).await,
            vec!["content".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn a_failure_before_any_content_still_ends_with_the_error_event() {
        assert_eq!(collect(vec![Err("reset")]).await, vec![interrupted()]);
    }

    #[tokio::test]
    async fn a_lost_trailer_ends_with_done() {
        assert_eq!(
            collect(vec![Ok("content"), Ok("finish"), Err("reset")]).await,
            ["content", "finish", "data: [DONE]\n\n"]
        );
        assert_eq!(
            collect(vec![Ok("content"), Ok("finish")]).await,
            ["content", "finish", "data: [DONE]\n\n"]
        );
    }

    #[tokio::test]
    async fn nothing_follows_a_terminal_event() {
        assert_eq!(
            collect(vec![Ok("content"), Ok("done"), Err("reset")]).await,
            ["content", "done"]
        );
    }

    #[test]
    fn every_choice_must_finish() {
        let mut progress = StreamProgress::default();
        progress.record_choice(0, true);
        progress.record_choice(1, false);
        assert!(!progress.generation_finished());
        progress.record_choice(1, true);
        assert!(progress.generation_finished());

        let mut usage_only = StreamProgress::default();
        usage_only.record_usage();
        assert!(
            usage_only.generation_finished(),
            "the usage chunk comes after every finish"
        );
    }

    #[test]
    fn the_error_event_is_an_embedded_error_envelope() {
        let event = interrupted_event();
        let data = std::str::from_utf8(&event)
            .unwrap()
            .strip_prefix("data: ")
            .unwrap()
            .trim_end();
        let value: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(value["error"]["code"], 502);
        assert_eq!(value["error"]["type"], "api_error");
        assert!(value["error"]["param"].is_null());
        assert!(
            data.starts_with("{\"error\""),
            "the batch reassembler matches this prefix"
        );
    }
}
