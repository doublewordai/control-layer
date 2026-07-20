//! Generic edge protocol-translation layer.
//!
//! A single Axum middleware (see [`middleware`]) is layered as the OUTERMOST
//! Tower layer on the onwards router. For each request it asks a registry of
//! [`ProtocolTranslator`]s whether any of them claims the request (by route +
//! headers, cheaply, with no body deserialisation). If one matches, the
//! middleware:
//!
//! 1. translates the foreign request body into canonical OpenAI Chat Completions
//!    and normalises the path (e.g. `/messages` -> `/chat/completions`),
//! 2. lets the request continue through the UNCHANGED onwards proxy path (so
//!    image normalisation, tool injection, logging, billing and routing all see
//!    Chat Completions),
//! 3. translates the response - both blocking bodies and, for `stream: true`,
//!    reframing the SSE stream - back into the foreign protocol before the
//!    bytes leave.
//!
//! Routing happens exactly once and this is NOT a re-routing rewrite. A real
//! route matches the request first - onwards' `/messages` alias to the
//! chat-completions handler in strict mode, the catch-all in non-strict - and
//! `Router::layer` runs this middleware AFTER that match. We then normalise the
//! path purely so the code that READS it downstream treats the request as plain
//! chat completions: the non-strict upstream forwarder (which derives the
//! upstream URL from the inbound path), the response sanitizer, and
//! image_normalizer. We do not, and cannot, rewrite the URI to bounce the
//! request back through the router - a nested router fixes its sub-path at match
//! time, so a post-match URI change never re-dispatches.
//!
//! Anthropic Messages (`/v1/messages`) is the first implementation and is purely
//! synchronous. The core transform stays sync and pure; a translator that needs
//! async, stateful work opts in via the [`ProtocolTranslator::pre_request`] /
//! [`ProtocolTranslator::post_response`] hooks, which the middleware awaits
//! around the pure translation. These brackets are how the planned OpenAI
//! Responses translator will resolve `previous_response_id` (hydration) and
//! persist the produced object, absorbing the translation half of the onwards
//! adapter. Multi-step tool-loop orchestration is still deliberately NOT part of
//! this layer.

pub mod anthropic;
pub mod middleware;
pub mod responses;

use axum::http::{HeaderMap, StatusCode, Uri, request::Parts};
use bytes::Bytes;
use std::sync::Arc;

/// Error raised while translating a request or response.
#[derive(Debug, thiserror::Error)]
pub enum TranslationError {
    /// The inbound foreign request was malformed (maps to a 400).
    #[error("{0}")]
    BadRequest(String),
    /// Something went wrong on our side translating (maps to a 5xx).
    #[error("{0}")]
    Internal(String),
}

/// The result of translating a foreign request into canonical Chat Completions.
///
/// The `uri` is normalised to the canonical chat-completions path (e.g.
/// `/messages` -> `/chat/completions`). This is NOT a re-routing rewrite: the
/// request has already been matched by a real route (onwards' `/messages` alias
/// in strict mode, the catch-all in non-strict), so routing happens exactly
/// once. We rewrite the path only so the code that READS it downstream - the
/// non-strict upstream forwarder, the response sanitizer, and image_normalizer -
/// treats the request as plain chat-completions. In strict mode the upstream
/// path is hardcoded by `chat_completions_handler`, so the rewrite is harmless
/// there; it is load-bearing for non-strict, where the upstream path is derived
/// from the inbound path.
pub struct TranslatedRequest {
    /// The normalised request URI (path now ends with `/chat/completions`).
    pub uri: Uri,
    /// The headers to forward downstream (auth normalised, stale length removed).
    pub headers: HeaderMap,
    /// The translated Chat Completions request body.
    pub body: Bytes,
}

/// A single foreign-protocol translator. Every translator is pure and stateless:
/// a synchronous request/response conversion with no I/O and no stored handles.
/// Stateful work for an API that has a control plane (e.g. OpenAI Responses'
/// `previous_response_id` hydration, id minting, and persistence) lives OUTSIDE
/// the translator - in the inference middleware (routing / id / hydration) and
/// the outlet (persistence) - so the translator stays a pure protocol converter.
pub trait ProtocolTranslator: Send + Sync {
    /// Stable name for logging/metrics.
    fn name(&self) -> &'static str;

    /// Cheap claim check over route + headers only. MUST NOT read or
    /// deserialise the body, so the fast path for native requests stays fast.
    fn detect(&self, path: &str, headers: &HeaderMap) -> bool;

    /// Translate the foreign request into a canonical Chat Completions request.
    fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError>;

    /// Translate a successful (blocking) Chat Completions response body back
    /// into the foreign protocol. `request` is the original inbound foreign
    /// request body, for protocols whose response echoes request fields (e.g.
    /// OpenAI Responses echoes `model` / `tools` / `instructions`); protocols
    /// that don't need it (Anthropic) ignore it. `response_id` is the platform's
    /// tracking id for this request (from `x-fusillade-request-id`, set by the
    /// inference middleware): protocols whose response carries a retrievable id
    /// that must match the stored record (OpenAI Responses) stamp it, so a later
    /// `GET /v1/responses/{id}` resolves; `None` when absent (unit tests, or a
    /// native path with no tracking row) - the translator then self-generates.
    fn translate_response(&self, request: &Bytes, response_id: Option<&str>, body: Bytes) -> Result<Bytes, TranslationError>;

    /// Translate an error response (any non-2xx) into the foreign error shape.
    fn translate_error(&self, status: StatusCode, body: Bytes) -> (StatusCode, Bytes);

    /// Build a fresh foreign-shaped error from a status and message, for
    /// failures detected at the edge (e.g. a malformed inbound request) before
    /// there is any downstream body to reshape.
    fn error_from_message(&self, status: StatusCode, message: &str) -> (StatusCode, Bytes);

    /// Create a fresh, stateful reframer for one streaming (SSE) response. The
    /// middleware feeds it each upstream Chat Completions chunk and forwards the
    /// foreign-protocol SSE bytes it emits. `request` is the original inbound
    /// foreign request body, for protocols whose streamed response echoes request
    /// fields (e.g. OpenAI Responses); protocols that don't need it ignore it.
    /// `response_id` is the platform tracking id (see `translate_response`), which
    /// a streamed foreign response stamps on its opening event's id.
    fn stream_reframer(&self, request: &Bytes, response_id: Option<&str>) -> Box<dyn StreamReframer>;
}

/// Stateful transformer that turns an OpenAI Chat Completions SSE stream into a
/// foreign-protocol typed event stream. One instance per response.
pub trait StreamReframer: Send {
    /// Feed one upstream chunk (the parsed `data:` JSON of a `chat.completion.chunk`).
    /// Returns the foreign SSE bytes to forward to the client (may be empty).
    fn push(&mut self, chunk: &serde_json::Value) -> Vec<u8>;

    /// The upstream stream ended abnormally (a transport error mid-stream). Emit a
    /// terminal foreign-protocol error event instead of a clean close. Idempotent.
    fn error(&mut self, message: &str) -> Vec<u8>;

    /// The upstream stream has ended; emit any closing events (idempotent).
    fn finish(&mut self) -> Vec<u8>;
}

/// Ordered registry of translators. First `detect()` match wins; no match
/// means the request passes through untouched (native Chat Completions path).
#[derive(Clone)]
pub struct TranslationRegistry {
    translators: Vec<Arc<dyn ProtocolTranslator>>,
    /// Cap on the inbound foreign request body the middleware will buffer before
    /// translating. `usize::MAX` means unlimited.
    max_body_size: usize,
}

impl Default for TranslationRegistry {
    fn default() -> Self {
        Self {
            translators: Vec::new(),
            max_body_size: usize::MAX,
        }
    }
}

impl TranslationRegistry {
    pub fn new(translators: Vec<Arc<dyn ProtocolTranslator>>) -> Self {
        Self {
            translators,
            max_body_size: usize::MAX,
        }
    }

    /// Set the maximum inbound body size the middleware will buffer (bytes).
    pub fn with_max_body_size(mut self, max_body_size: usize) -> Self {
        self.max_body_size = max_body_size;
        self
    }

    pub fn max_body_size(&self) -> usize {
        self.max_body_size
    }

    /// Return the first translator that claims this request, if any.
    pub fn detect(&self, path: &str, headers: &HeaderMap) -> Option<Arc<dyn ProtocolTranslator>> {
        self.translators.iter().find(|t| t.detect(path, headers)).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.translators.is_empty()
    }
}
