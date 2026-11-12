//! First-token detection for the reference cache's provisional write: tells the cache
//! layer when a streaming chat completion produced its first output, so the request's
//! prefixes become visible then instead of at completion.
//!
//! "First token" is the first SSE event carrying a `"choices"` member. An event carrying
//! `"error"` before that disarms the detector, so a stream that fails before producing
//! output writes nothing. Events are reassembled across body frames and judged in order,
//! so a split marker is still found and an error coalesced ahead of output still disarms.
//! The detector only inspects copies of the frames; the stream itself is never altered.

use bytes::BytesMut;

/// Bytes of an unfinished event held while waiting for its end. Opening events are small;
/// past this the detector gives up and the completion write makes the request visible.
const MAX_PENDING_EVENT: usize = 64 * 1024;

/// Watches a Chat Completions SSE stream for its first output.
pub struct FirstTokenDetector {
    armed: bool,
    /// The unfinished event so far, while armed.
    pending: BytesMut,
}

impl Default for FirstTokenDetector {
    fn default() -> Self {
        Self {
            armed: true,
            pending: BytesMut::new(),
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// End of the first complete event in `buf` (past its blank-line terminator).
fn event_end(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl FirstTokenDetector {
    /// Feed the next body frame. True exactly once: on the frame that completes the
    /// first event with output, unless an error event came first.
    pub fn observe(&mut self, data: &[u8]) -> bool {
        if !self.armed {
            return false;
        }
        self.pending.extend_from_slice(data);
        let mut fired = false;
        while self.armed
            && let Some(end) = event_end(&self.pending)
        {
            let event = self.pending.split_to(end);
            if contains(&event, b"\"choices\"") {
                fired = true;
                self.armed = false;
            } else if contains(&event, b"\"error\"") {
                self.armed = false;
            }
        }
        if !self.armed || self.pending.len() > MAX_PENDING_EVENT {
            self.armed = false;
            self.pending = BytesMut::new();
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(frames: &[&str]) -> usize {
        let mut d = FirstTokenDetector::default();
        frames.iter().filter(|f| d.observe(f.as_bytes())).count()
    }

    #[test]
    fn fires_once_on_the_first_chunk_with_output() {
        let frames = [
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let mut d = FirstTokenDetector::default();
        let fired_at: Vec<bool> = frames.iter().map(|f| d.observe(f.as_bytes())).collect();
        assert_eq!(fired_at, vec![false, true, false, false]);
    }

    #[test]
    fn an_error_before_any_output_disarms() {
        assert_eq!(
            fired(&["data: {\"error\":{\"message\":\"overloaded\"}}\n\n", "data: {\"choices\":[]}\n\n"]),
            0
        );
    }

    #[test]
    fn an_error_after_output_does_not_undo_it() {
        assert_eq!(
            fired(&["data: {\"choices\":[]}\n\n", "data: {\"error\":{\"message\":\"cut\"}}\n\n"]),
            1
        );
    }

    #[test]
    fn an_event_split_across_frames_still_fires() {
        let mut d = FirstTokenDetector::default();
        assert!(!d.observe(b"data: {\"cho"));
        assert!(!d.observe(b"ices\":[{\"delta\":{\"content\":\"x\"}}]}"));
        assert!(d.observe(b"\r\n\r\n"), "fires on the frame that completes the event");
    }

    #[test]
    fn an_error_coalesced_ahead_of_output_disarms() {
        assert_eq!(
            fired(&["data: {\"error\":{\"message\":\"overloaded\"}}\n\ndata: {\"choices\":[]}\n\n"]),
            0
        );
    }

    #[test]
    fn an_unterminated_oversized_event_gives_up() {
        let big = format!("data: {{\"pad\":\"{}\"", "x".repeat(MAX_PENDING_EVENT));
        assert_eq!(fired(&[&big, ",\"choices\":[]}\n\n"]), 0, "left to the completion write");
    }
}
