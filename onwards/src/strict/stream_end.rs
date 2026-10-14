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
//! When every expected choice had already finished and only the trailer was
//! lost, the stream ends with `[DONE]` instead. Nothing is forwarded after a
//! terminal event.

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
#[derive(Debug)]
pub(crate) struct StreamProgress {
    /// Choices the request asked for (`n`).
    expected_choices: u32,
    /// A terminal event was forwarded: `[DONE]`, an error, or a final
    /// Responses event.
    ended: bool,
    /// Each choice index seen, and whether it has a `finish_reason`.
    choices: Vec<(u32, bool)>,
    /// The usage-only chunk that follows every finished choice arrived.
    saw_final_usage: bool,
}

impl StreamProgress {
    pub(crate) fn new(expected_choices: u32) -> Self {
        Self {
            expected_choices: expected_choices.max(1),
            ended: false,
            choices: Vec::new(),
            saw_final_usage: false,
        }
    }

    pub(crate) fn record_end(&mut self) {
        self.ended = true;
    }

    pub(crate) fn record_choice(&mut self, index: u32, finished: bool) {
        match self.choices.iter_mut().find(|(i, _)| *i == index) {
            Some((_, done)) => *done |= finished,
            None => self.choices.push((index, finished)),
        }
    }

    /// Record a chunk with usage and no choices. Usage on a content chunk
    /// (`continuous_usage_stats`) says nothing about whether the answer ended.
    pub(crate) fn record_final_usage(&mut self) {
        self.saw_final_usage = true;
    }

    /// Every expected choice has finished, or the final usage was reported.
    fn generation_finished(&self) -> bool {
        self.saw_final_usage
            || (self.choices.len() >= self.expected_choices as usize
                && self.choices.iter().all(|(_, done)| *done))
    }

    /// The event that closes a stream whose upstream stopped here, if any.
    fn closing_event(&self) -> Option<Bytes> {
        if self.ended {
            None
        } else if self.generation_finished() {
            debug!("Upstream stream finished without [DONE]; adding it");
            Some(Bytes::from_static(DONE_EVENT))
        } else {
            warn!("Upstream stream ended before it finished; ending it with an error event");
            Some(interrupted_event())
        }
    }
}

/// Newlines missing after `bytes` for its last SSE event to be complete.
fn missing_separator(bytes: &[u8]) -> usize {
    2 - bytes
        .iter()
        .rev()
        .take(2)
        .take_while(|b| **b == b'\n')
        .count()
}

/// Run each upstream event through `sanitize`, and end the stream explicitly
/// if the upstream fails or stops before a terminal event.
///
/// `sanitize` returns an error for an event it cannot forward; that ends the
/// stream the same way as an upstream failure. The stream never yields an
/// error, so the client always receives a complete response body, and its
/// last event is always terminated.
pub(crate) fn end_explicitly<S, E, F>(
    upstream: S,
    mut progress: StreamProgress,
    mut sanitize: F,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnMut(Bytes, &mut StreamProgress) -> Result<Bytes, io::Error> + Send + 'static,
{
    async_stream::stream! {
        let mut upstream = upstream;
        // An unterminated final event is flushed at EOF without its blank
        // line; anything appended after it must not run into it.
        let mut missing = 0;
        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    error!(error = %e, "Stream error");
                    break;
                }
            };
            let Ok(bytes) = sanitize(chunk, &mut progress) else {
                break;
            };
            if !bytes.is_empty() {
                missing = missing_separator(&bytes);
            }
            yield Ok(bytes);
            if progress.ended {
                break;
            }
        }
        if missing > 0 {
            yield Ok(Bytes::from_static(&b"\n\n"[..missing]));
        }
        if let Some(event) = progress.closing_event() {
            yield Ok(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    async fn collect_n(
        expected_choices: u32,
        chunks: Vec<Result<&'static str, &'static str>>,
    ) -> Vec<String> {
        let upstream = stream::iter(
            chunks
                .into_iter()
                .map(|c| c.map(|s| Bytes::from_static(s.as_bytes())))
                .collect::<Vec<_>>(),
        );
        let out = end_explicitly(
            upstream,
            StreamProgress::new(expected_choices),
            |chunk, progress| {
                match std::str::from_utf8(&chunk).unwrap().trim_end() {
                    "done" => progress.record_end(),
                    "finish" => progress.record_choice(0, true),
                    "finish 1" => progress.record_choice(1, true),
                    "content" => progress.record_choice(0, false),
                    "usage" => progress.record_final_usage(),
                    "bad" => return Err(io::Error::other("bad")),
                    _ => {}
                }
                Ok(chunk)
            },
        );
        out.map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await
    }

    async fn collect(chunks: Vec<Result<&'static str, &'static str>>) -> Vec<String> {
        collect_n(1, chunks).await
    }

    fn interrupted() -> String {
        String::from_utf8(interrupted_event().to_vec()).unwrap()
    }

    const DONE: &str = "data: [DONE]\n\n";

    #[tokio::test]
    async fn a_complete_stream_is_unchanged() {
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("finish\n\n"), Ok("done\n\n")]).await,
            ["content\n\n", "finish\n\n", "done\n\n"]
        );
    }

    #[tokio::test]
    async fn an_upstream_failure_mid_answer_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content\n\n"), Err("reset")]).await,
            vec!["content\n\n".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn an_early_end_mid_answer_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content\n\n")]).await,
            vec!["content\n\n".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn an_unforwardable_event_ends_with_the_error_event() {
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("bad\n\n"), Ok("content\n\n")]).await,
            vec!["content\n\n".to_string(), interrupted()]
        );
    }

    #[tokio::test]
    async fn a_failure_before_any_content_still_ends_with_the_error_event() {
        assert_eq!(collect(vec![Err("reset")]).await, vec![interrupted()]);
    }

    #[tokio::test]
    async fn a_lost_trailer_ends_with_done() {
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("finish\n\n"), Err("reset")]).await,
            ["content\n\n", "finish\n\n", DONE]
        );
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("usage\n\n")]).await,
            ["content\n\n", "usage\n\n", DONE],
            "the usage-only chunk comes after every finish"
        );
    }

    #[tokio::test]
    async fn nothing_is_forwarded_after_a_terminal_event() {
        assert_eq!(
            collect(vec![
                Ok("content\n\n"),
                Ok("done\n\n"),
                Ok("content\n\n"),
                Err("reset")
            ])
            .await,
            ["content\n\n", "done\n\n"]
        );
    }

    #[tokio::test]
    async fn every_requested_choice_must_finish() {
        assert_eq!(
            collect_n(2, vec![Ok("content\n\n"), Ok("finish\n\n"), Err("reset")]).await,
            vec![
                "content\n\n".to_string(),
                "finish\n\n".to_string(),
                interrupted()
            ],
            "choice 1 never arrived"
        );
        assert_eq!(
            collect_n(2, vec![Ok("finish\n\n"), Ok("finish 1\n\n"), Err("reset")]).await,
            ["finish\n\n", "finish 1\n\n", DONE]
        );
    }

    #[tokio::test]
    async fn an_unterminated_last_event_is_closed_before_anything_follows() {
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("content")]).await,
            vec![
                "content\n\n".to_string(),
                "content".to_string(),
                "\n\n".to_string(),
                interrupted()
            ]
        );
        assert_eq!(
            collect(vec![Ok("content\n\n"), Ok("done\n")]).await,
            ["content\n\n", "done\n", "\n"],
            "a terminal event is completed too"
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
