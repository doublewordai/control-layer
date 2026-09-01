//! Out-of-band functional request parameters — the single parse (COR-614).
//!
//! Coding-agent harnesses and proxy middlemen can usually configure exactly two things:
//! `base_url` and `model_name`. They cannot add fields to a request body. That puts
//! `service_tier` and the automatic-caching marker — both body fields — out of reach for
//! precisely the clients most likely to want them.
//!
//! This module accepts those parameters through the two channels such clients *do* control,
//! and normalises them into the body at the edge:
//!
//! - a **URL query parameter**: `?serviceTier=flex`, `?cacheBreakpoint=lastUserMessage`
//! - a **suffix on the model name**: `zai/glm-5.2-fp8-dw-flex`, in the spirit of the convention
//!   OpenRouter established with `deepseek/deepseek-r1:free`, but on our own `-dw-` delimiter
//!   rather than a colon — see "Why `-dw-` and not `:`" below
//!
//! The design rule is the one already written into [`crate::prompt_cache::query`]: translate
//! the out-of-band channel into the field the client could have sent itself, before anything
//! else looks at the body. Every downstream layer — logging, translation, caching, the flex
//! queue, onwards — keeps reading the body exactly as it does today and sees a request
//! indistinguishable from one that arrived with the field set. One code path, not two.
//!
//! That choice is load-bearing rather than cosmetic. The queued flex/background path strips
//! `service_tier` from the outbound body in two places ([`crate::inference::zdr`] and
//! fusillade's `sanitize_outbound_body`); both work on a *real body field*. Threading the
//! tier through a request extension instead would have sailed straight past both sanitisers
//! and put `service_tier` on the wire to the provider.
//!
//! ## Precedence: body field > model suffix > query param
//!
//! - **Body wins.** Already the shipped rule for `cacheBreakpoint` (`Inject::BodyFieldWins`):
//!   an end client that explicitly opted in *through* a param-appending proxy keeps its
//!   explicit behaviour. A `null` body field means "unset" and is overwritten.
//! - **Suffix beats query.** The suffix is typed per-request into a harness's model field;
//!   the query param is typically baked into an intermediary's `base_url` and applies to all
//!   of its traffic. The narrower scope wins.
//!
//! ## Strictness
//!
//! An unrecognised query param *name* is ignored — query strings routinely carry unrelated
//! params. An unrecognised *value* on a param we own is a 400, never a silent downgrade:
//! a typo must not leave the caller believing they selected a tier they didn't get. This
//! mirrors `invalid_cache_breakpoint`.
//!
//! A model-name suffix splits on whether the *base* resolves. `known-alias-dw-turbo` is a 400
//! naming the supported suffixes — the delimiter makes the intent unambiguous, so a typo
//! shouldn't masquerade as a missing model. `unknown-model-dw-turbo` is left untouched and gets
//! the usual `model_not_found` 404, which covers both an ordinary typo and a suffix aimed at a
//! model this deployment doesn't host.
//!
//! ## Why `-dw-` and not `:`
//!
//! The colon is already spoken for. Coding-agent harnesses use `model:token` for their own
//! per-request knobs — thinking strength (`:xhigh`, `:high`) most commonly — and OpenRouter
//! ships `:free` / `:nitro` variants. A colon delimiter would put us inside a namespace we
//! don't own: `some-model:xhigh` arriving at a deployment that hosts `some-model` would be
//! read as a deliberate-but-unrecognised suffix and rejected with a 400, breaking a harness
//! that was only ever talking to itself. Colons are also real *inside* model ids — Bedrock
//! (`anthropic.claude-v2:0`), Ollama (`llama3.1:8b`).
//!
//! `-dw-` is vendor-marked and effectively unused, so a name carrying it is almost certainly
//! addressed to us. Under the default delimiter every colon form above is inert: it never
//! matches, so the name is passed through untouched and resolves — or 404s — exactly as it
//! does today. The delimiter stays configurable for a deployment whose aliases already
//! contain `-dw-`.
//!
//! ## Why the model suffix is safe
//!
//! Aliases have no character-class validation anywhere — migration
//! `019_enforce_alias_uniqueness.sql` actively *generates* aliases containing spaces and
//! parentheses — so no delimiter is safe by construction, `-dw-` included.
//! [`resolve_model`] therefore never rewrites a model on the strength of the delimiter alone:
//!
//! 1. an exact alias match wins unconditionally, before any splitting, so an alias that
//!    genuinely *is* `vendor/model-dw-flex` routes to itself;
//! 2. a suffix is only stripped when what remains is itself a real routing alias.
//!
//! An unknown model, a typo, or a mid-reload alias table all fall through to "leave it
//! alone". There is no input for which this rewrites a model that would otherwise have
//! routed successfully.
//!
//! ## Multi-replica deployments
//!
//! `model_suffix` is per-process config, so it can differ between pods during a rollout. That
//! is safe but not invisible, and the distinction matters when reading a dashboard:
//!
//! - **Nothing can be left in a bad state.** Everything durable is written *post-strip* — the
//!   submit path persists the mutated body and the resolved alias, ZDR encrypts that same
//!   value, and `requests.model` stays plaintext. The daemon path never re-parses at all: its
//!   loopback dispatch always carries `x-fusillade-request-id`, which short-circuits the
//!   middleware before the body is read. So a pod with the flag off executes a flag-on pod's
//!   queued request correctly.
//! - **The symptom is submit-time only.** `my-model-dw-flex` is a flex request on a flag-on pod
//!   and a 404 on a flag-off one. [`record_suffix_ignored`] counts the second case so a
//!   partial rollout shows up as a metric rather than as unexplained 404s.
//!
//! Treat the flag as environment-wide: enable it on every replica or none.

use std::collections::HashSet;

use axum::http::Uri;
use axum::http::uri::PathAndQuery;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::prompt_cache::query::{CACHE_BREAKPOINT_PARAM, Inject, LAST_USER_MESSAGE, inject_marker, last_user_message_marker};

/// The `service_tier` query parameter. Camel-cased to match `cacheBreakpoint`, which
/// established the convention for this family of params.
pub const SERVICE_TIER_PARAM: &str = "serviceTier";

/// Accepted `service_tier` values, stored and re-emitted with the caller's exact spelling.
///
/// The realtime spellings are deliberately **not** collapsed to a canonical one. The realtime
/// path forwards `service_tier` upstream untouched, providers distinguish `priority` from
/// `default`, and `resolve_service_tier` is the single authority on which of these route as
/// realtime. Rewriting `priority` to `auto` here would silently change what the provider sees.
const SERVICE_TIER_VALUES: &[&str] = &["auto", "default", "priority", "flex", "background"];

/// Where a parameter came from. Determines precedence, and lets an error message name the
/// channel the caller actually used rather than a body field they never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The request body — the client set the field itself.
    Body,
    /// A suffix on the `model` field.
    ModelSuffix,
    /// A URL query parameter.
    Query,
}

impl Source {
    /// How to refer to this channel in a client-facing error.
    fn describe(self) -> &'static str {
        match self {
            Self::Body => "request body",
            Self::ModelSuffix => "model-name suffix",
            Self::Query => "query parameter",
        }
    }

    /// Stable metric label.
    fn as_label(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::ModelSuffix => "model_suffix",
            Self::Query => "query",
        }
    }
}

/// An out-of-band parameter was resolved from a channel other than the body.
///
/// The adoption signal for this feature: it shows a harness fleet picking up the suffix or the
/// query param without anyone having to read request logs. Body-sourced values are deliberately
/// not counted — they are ordinary traffic, and counting them would swamp the signal.
pub fn record_param(param: &'static str, source: Source) {
    metrics::counter!("dwctl_request_params_total", "param" => param, "source" => source.as_label()).increment(1);
}

/// A request carried a model-name suffix that would have resolved, but the channel is off on
/// this process.
///
/// The rollout-skew alarm. `model_suffix` is per-process config, so on a multi-replica
/// deployment it can be on for some pods and off for others; this counter is what makes that
/// visible, instead of clients seeing the same request succeed or 404 depending on where it
/// landed. A non-zero value on any pod means finish the rollout (or roll it back) — see the
/// module docs for why nothing is left in a bad state meanwhile.
pub fn record_suffix_ignored() {
    metrics::counter!("dwctl_request_params_suffix_ignored_total").increment(1);
}

/// The automatic-caching marker, requested out of band. One variant today; the enum exists so
/// a second breakpoint kind doesn't change this module's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheBreakpoint {
    /// Mark the last cacheable block of the request — [`LAST_USER_MESSAGE`].
    LastUserMessage,
}

/// Which out-of-band channels are recognised for this deployment.
#[derive(Debug, Clone)]
pub struct ParamsConfig {
    /// Honour owned query parameters.
    pub query_params: bool,
    /// Honour a functional suffix on the `model` field.
    pub model_suffix: bool,
    /// The delimiter separating the alias from its suffix tokens.
    pub model_suffix_delimiter: String,
    /// Whether `cacheBreakpoint` is recognised at all. Derived from `config.cache.enabled`,
    /// never user-set: the cache layer is only in the stack when that flag is on, and an
    /// injected `cache_control` with no cache layer to strip it would leak upstream.
    pub cache_breakpoint: bool,
}

impl Default for ParamsConfig {
    fn default() -> Self {
        Self {
            query_params: true,
            model_suffix: false,
            model_suffix_delimiter: "-dw-".to_string(),
            cache_breakpoint: false,
        }
    }
}

/// Live alias membership, so a suffix is only stripped when what remains actually routes.
///
/// Implemented for `onwards::target::Targets` in production — the very map onwards routes
/// against, so this can't disagree with the router — and for `HashSet<String>` in tests.
pub trait AliasIndex {
    fn contains(&self, alias: &str) -> bool;
}

impl AliasIndex for onwards::target::Targets {
    fn contains(&self, alias: &str) -> bool {
        self.targets.contains_key(alias)
    }
}

impl AliasIndex for HashSet<String> {
    fn contains(&self, alias: &str) -> bool {
        HashSet::contains(self, alias)
    }
}

/// One request's resolved functional parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestParams {
    /// `model` with any functional suffix removed — the alias to route on. `None` when the
    /// body has no usable `model`, or when nothing was stripped.
    pub model: Option<String>,
    /// `model` exactly as the client sent it, kept for telemetry.
    pub requested_model: Option<String>,
    /// The resolved `service_tier`, in the spelling to write into the body.
    pub service_tier: Option<(&'static str, Source)>,
    /// The resolved automatic-caching breakpoint.
    pub cache_breakpoint: Option<(CacheBreakpoint, Source)>,
    /// Query parameters this module consumed, to be stripped from the forwarded URI.
    ///
    /// Only ever names params we actually recognised: when `cacheBreakpoint` is disabled it
    /// is left in the query untouched, so a deployment without the cache layer behaves as if
    /// this module didn't exist.
    pub consumed_query_params: Vec<&'static str>,
    /// The model carried a suffix that **would** have resolved, but the channel is off on this
    /// process.
    ///
    /// This is the multi-replica rollout signal. `model_suffix` is per-process config, so
    /// during a partial rollout the same request is a flex request on one pod and a 404 on
    /// another. Nothing persists in a bad state (see the module docs), but without this the
    /// only symptom is mystery 404s. The caller turns it into a counter.
    pub suffix_ignored: bool,
}

impl RequestParams {
    /// Whether the model was rewritten (a suffix was stripped).
    fn model_changed(&self) -> bool {
        match (&self.model, &self.requested_model) {
            (Some(resolved), Some(requested)) => resolved != requested,
            _ => false,
        }
    }
}

/// A request we must reject rather than silently misinterpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidParam {
    /// A parameter we own carried a value we don't support.
    Value {
        param: &'static str,
        value: String,
        supported: &'static [&'static str],
        source: Source,
    },
    /// The model name carried a suffix token we don't recognise, on a base that **is** a real
    /// alias. The delimiter makes the intent unambiguous there, so naming the supported set is
    /// more useful than the `model_not_found` the caller would otherwise get. When the base is
    /// *not* an alias this is never raised — that case is an ordinary unknown model.
    UnknownSuffix { token: String },
}

impl std::fmt::Display for InvalidParam {
    /// A short, log-safe summary. Carries only client-supplied identifiers — a parameter name
    /// and the offending token — never body content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value { param, value, source, .. } => {
                write!(f, "invalid {param} value {value:?} from the {}", source.describe())
            }
            Self::UnknownSuffix { token } => write!(f, "unknown model suffix {token:?}"),
        }
    }
}

/// One recognised suffix token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamToken {
    ServiceTier(&'static str),
    CacheBreakpoint(CacheBreakpoint),
}

impl ParamToken {
    /// Whether two tokens set the same parameter — used to reject a contradictory model name.
    fn same_param(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::ServiceTier(_), Self::ServiceTier(_)) | (Self::CacheBreakpoint(_), Self::CacheBreakpoint(_))
        )
    }
}

/// The bare suffix tokens. Values are bare rather than `name=value` because users type these
/// by hand into a harness's model field; the explicit `name=value` form is still accepted by
/// [`lookup_token`] as an escape hatch for future params whose values would collide.
const SUFFIX_TOKENS: &[(&str, ParamToken)] = &[
    ("flex", ParamToken::ServiceTier("flex")),
    ("background", ParamToken::ServiceTier("background")),
    ("priority", ParamToken::ServiceTier("priority")),
    ("cache", ParamToken::CacheBreakpoint(CacheBreakpoint::LastUserMessage)),
];

/// The outcome of matching one suffix token.
enum TokenLookup {
    Found(ParamToken),
    /// A recognised param name with an unsupported value (explicit `name=value` form only).
    BadValue {
        param: &'static str,
        value: String,
        supported: &'static [&'static str],
    },
    /// Not one of ours — almost always just part of the model name.
    Unknown,
}

/// Match one suffix token, accepting both the bare (`flex`) and explicit
/// (`serviceTier=flex`) forms.
fn lookup_token(tail: &str) -> TokenLookup {
    if let Some((name, value)) = tail.split_once('=') {
        return match name {
            SERVICE_TIER_PARAM => match service_tier_value(value) {
                Some(v) => TokenLookup::Found(ParamToken::ServiceTier(v)),
                None => TokenLookup::BadValue {
                    param: SERVICE_TIER_PARAM,
                    value: value.to_string(),
                    supported: SERVICE_TIER_VALUES,
                },
            },
            CACHE_BREAKPOINT_PARAM => match cache_breakpoint_value(value) {
                Some(v) => TokenLookup::Found(ParamToken::CacheBreakpoint(v)),
                None => TokenLookup::BadValue {
                    param: CACHE_BREAKPOINT_PARAM,
                    value: value.to_string(),
                    supported: &[LAST_USER_MESSAGE],
                },
            },
            _ => TokenLookup::Unknown,
        };
    }
    match SUFFIX_TOKENS.iter().find(|(token, _)| *token == tail) {
        Some((_, token)) => TokenLookup::Found(*token),
        None => TokenLookup::Unknown,
    }
}

/// Validate a `service_tier` value, returning the canonical `&'static str` to write.
fn service_tier_value(value: &str) -> Option<&'static str> {
    SERVICE_TIER_VALUES.iter().find(|v| **v == value).copied()
}

/// Validate a `cacheBreakpoint` value. Byte-exact, like [`crate::prompt_cache::query`].
fn cache_breakpoint_value(value: &str) -> Option<CacheBreakpoint> {
    (value == LAST_USER_MESSAGE).then_some(CacheBreakpoint::LastUserMessage)
}

/// Split a model name into its routing alias and any functional suffix tokens.
///
/// See the module docs for why this is conservative. In short: an exact alias match wins
/// before any splitting, and a suffix is only stripped when what remains is itself an alias.
///
/// A name that sets the same parameter twice (`alias-flex-background`) is contradictory and is
/// left untouched rather than resolved to an arbitrary half — the request then 404s as an
/// unknown model, which is the honest outcome.
fn resolve_model<'a>(model: &'a str, delimiter: &str, aliases: &dyn AliasIndex) -> Result<(&'a str, Vec<ParamToken>), InvalidParam> {
    // An exact match always wins, unconditionally, before any splitting.
    if aliases.contains(model) {
        return Ok((model, Vec::new()));
    }
    if delimiter.is_empty() {
        return Ok((model, Vec::new()));
    }

    let mut head = model;
    let mut tokens: Vec<ParamToken> = Vec::new();
    while let Some((rest, tail)) = head.rsplit_once(delimiter) {
        match lookup_token(tail) {
            TokenLookup::Found(token) => {
                if tokens.iter().any(|t| t.same_param(token)) {
                    break;
                }
                tokens.push(token);
                head = rest;
                // The stripped candidate must itself be a real routing target.
                if aliases.contains(head) {
                    return Ok((head, tokens));
                }
            }
            // A recognised param name with a bad value is only unambiguous — and therefore
            // only worth a 400 — when what remains is a real alias. Otherwise this is an
            // ordinary unknown model and 404 is the better error.
            TokenLookup::BadValue { param, value, supported } => {
                if aliases.contains(rest) {
                    return Err(InvalidParam::Value {
                        param,
                        value,
                        supported,
                        source: Source::ModelSuffix,
                    });
                }
                break;
            }
            // The delimiter signals deliberate intent, so when the remainder is a live alias
            // we know a suffix was meant and can name the supported set. An unknown base is
            // left alone and 404s — that covers a plain typo, and an OpenRouter-style
            // `:free` on a model this deployment simply doesn't host.
            TokenLookup::Unknown => {
                if aliases.contains(rest) {
                    return Err(InvalidParam::UnknownSuffix { token: tail.to_string() });
                }
                break;
            }
        }
    }
    Ok((model, Vec::new()))
}

/// Find the first occurrence of a query parameter. First-wins matches
/// [`crate::prompt_cache::query`]; the name is matched byte-exact, so a differently-cased
/// spelling is a different (ignored) param, per query-string convention.
fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then_some(value)
    })
}

/// Parse one request's functional parameters from its body, query string, and model name.
///
/// Pure: reads, never mutates. [`apply_to_body`] does the writing.
pub fn parse(body: &Value, query: Option<&str>, aliases: &dyn AliasIndex, cfg: &ParamsConfig) -> Result<RequestParams, InvalidParam> {
    let mut params = RequestParams {
        requested_model: body.get("model").and_then(Value::as_str).map(str::to_string),
        ..Default::default()
    };

    // ── Query parameters (lowest precedence) ────────────────────────────────
    if cfg.query_params
        && let Some(query) = query
    {
        if let Some(value) = query_value(query, SERVICE_TIER_PARAM) {
            let Some(tier) = service_tier_value(value) else {
                return Err(InvalidParam::Value {
                    param: SERVICE_TIER_PARAM,
                    value: value.to_string(),
                    supported: SERVICE_TIER_VALUES,
                    source: Source::Query,
                });
            };
            params.service_tier = Some((tier, Source::Query));
            params.consumed_query_params.push(SERVICE_TIER_PARAM);
        }
        // Only recognised when the cache layer is actually in the stack; otherwise the param
        // is left in the query and passes through as an unknown param, exactly as today.
        if cfg.cache_breakpoint
            && let Some(value) = query_value(query, CACHE_BREAKPOINT_PARAM)
        {
            let Some(breakpoint) = cache_breakpoint_value(value) else {
                return Err(InvalidParam::Value {
                    param: CACHE_BREAKPOINT_PARAM,
                    value: value.to_string(),
                    supported: &[LAST_USER_MESSAGE],
                    source: Source::Query,
                });
            };
            params.cache_breakpoint = Some((breakpoint, Source::Query));
            params.consumed_query_params.push(CACHE_BREAKPOINT_PARAM);
        }
    }

    // ── Model-name suffix (beats the query) ─────────────────────────────────
    // Cloned so the resolved slice doesn't hold a borrow of `params` across the writes below.
    if cfg.model_suffix
        && let Some(requested) = params.requested_model.clone()
    {
        let (resolved, tokens) = resolve_model(&requested, &cfg.model_suffix_delimiter, aliases)?;
        if !tokens.is_empty() {
            params.model = Some(resolved.to_string());
            for token in tokens {
                match token {
                    ParamToken::ServiceTier(tier) => params.service_tier = Some((tier, Source::ModelSuffix)),
                    ParamToken::CacheBreakpoint(bp) => params.cache_breakpoint = Some((bp, Source::ModelSuffix)),
                }
            }
        }
    } else if let Some(requested) = params.requested_model.as_deref() {
        // Channel off: still run the resolution — one `DashMap` hit and a `rsplit` — purely to
        // notice a request that WOULD have used it, which is how a partial rollout becomes
        // visible instead of showing up as unexplained 404s. Errors are deliberately swallowed:
        // a disabled channel must never reject a request, so the `UnknownSuffix` 400 above
        // cannot fire here.
        params.suffix_ignored = matches!(
            resolve_model(requested, &cfg.model_suffix_delimiter, aliases),
            Ok((_, tokens)) if !tokens.is_empty()
        );
    }

    // ── Body fields (highest precedence) ────────────────────────────────────
    // A `null` field is "unset" and does not win, matching `inject_marker`'s treatment of a
    // null `cache_control`.
    if let Some(tier) = body.get("service_tier").and_then(Value::as_str)
        && let Some(tier) = service_tier_value(tier)
    {
        params.service_tier = Some((tier, Source::Body));
    }
    if body.get("cache_control").is_some_and(|v| !v.is_null()) {
        params.cache_breakpoint = Some((CacheBreakpoint::LastUserMessage, Source::Body));
    }

    Ok(params)
}

/// Write the resolved parameters into the body, so every inner layer sees a request identical
/// to one the client could have sent itself.
///
/// Returns `true` iff anything changed. The caller re-serialises only then, which is what
/// keeps the default request path byte-identical to today.
pub fn apply_to_body(params: &RequestParams, body: &mut Value) -> bool {
    if !body.is_object() {
        // onwards will reject a non-object body downstream; nothing to inject into.
        return false;
    }
    let mut changed = false;

    if params.model_changed()
        && let Some(model) = &params.model
        && let Some(object) = body.as_object_mut()
    {
        object.insert("model".to_string(), Value::String(model.clone()));
        changed = true;
    }

    // Only write what came from out of band — a body-sourced value is already there.
    if let Some((tier, source)) = params.service_tier
        && source != Source::Body
        && let Some(object) = body.as_object_mut()
    {
        object.insert("service_tier".to_string(), Value::String(tier.to_string()));
        changed = true;
    }

    if let Some((CacheBreakpoint::LastUserMessage, source)) = params.cache_breakpoint
        && source != Source::Body
        && matches!(inject_marker(body, last_user_message_marker()), Inject::Applied)
    {
        changed = true;
    }

    changed
}

/// Remove every named parameter from a URI's query, preserving the others in order and
/// dropping the `?` entirely if nothing remains.
///
/// Must run before forwarding: onwards sends `path_and_query` verbatim upstream, so a param
/// we own must not leak to the provider. Generalises the single-param
/// `crate::prompt_cache::query::strip_param`, which now delegates here.
pub fn strip_params(uri: &Uri, names: &[&str]) -> Uri {
    fn param_key(pair: &str) -> &str {
        pair.split_once('=').map(|(k, _)| k).unwrap_or(pair)
    }
    let Some(query) = uri.query() else { return uri.clone() };
    if names.is_empty() || !query.split('&').any(|p| names.contains(&param_key(p))) {
        return uri.clone();
    }
    let kept: Vec<&str> = query.split('&').filter(|p| !names.contains(&param_key(p))).collect();
    let path_and_query = if kept.is_empty() {
        uri.path().to_string()
    } else {
        format!("{}?{}", uri.path(), kept.join("&"))
    };
    // The path and the kept pairs are substrings of an already-valid URI, so this re-parse
    // can't fail; if it somehow does, keep the original URI (an unknown upstream query param)
    // rather than failing the request.
    let mut parts = uri.clone().into_parts();
    match path_and_query.parse::<PathAndQuery>() {
        Ok(pq) => parts.path_and_query = Some(pq),
        Err(_) => return uri.clone(),
    }
    Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
}

/// Structured 400 for an unsupported value on a parameter we own — strict, like
/// `invalid_cache_breakpoint`: a typo must not silently select a different tier while the
/// caller believes theirs applied. Names the channel used, so the message never points at a
/// body field the client never sent.
pub fn rejection_response(error: &InvalidParam) -> Response {
    let (message, param) = match error {
        InvalidParam::Value {
            param,
            value,
            supported,
            source,
        } => (
            format!(
                "invalid {} value {:?} in the {}; supported values: {}",
                param,
                value,
                source.describe(),
                quoted_list(supported.iter().copied()),
            ),
            *param,
        ),
        // The supported set is derived from `SUFFIX_TOKENS` rather than restated, so the
        // message can't drift from what the parser actually accepts.
        InvalidParam::UnknownSuffix { token } => (
            format!(
                "unknown model suffix {:?}; supported suffixes: {}",
                token,
                quoted_list(SUFFIX_TOKENS.iter().map(|(name, _)| *name)),
            ),
            "model",
        ),
    };
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "code": "invalid_request_param",
            "param": param,
        }
    });
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

/// `a`, `b`, `c` → `"a", "b", "c"` — the shape both messages above use.
fn quoted_list<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values.map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn aliases(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn cfg() -> ParamsConfig {
        ParamsConfig {
            query_params: true,
            model_suffix: true,
            model_suffix_delimiter: "-dw-".into(),
            cache_breakpoint: true,
        }
    }

    // ── resolve_model ───────────────────────────────────────────────────────

    #[test]
    fn exact_alias_wins_even_when_it_looks_suffixed() {
        // The whole safety property: an alias that genuinely ends in a token is never split.
        let index = aliases(&["m-dw-flex", "m"]);
        let (model, tokens) = resolve_model("m-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "m-dw-flex");
        assert!(tokens.is_empty());
    }

    #[test]
    fn strips_when_only_the_base_exists() {
        let index = aliases(&["m"]);
        let (model, tokens) = resolve_model("m-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "m");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("flex")]);
    }

    #[test]
    fn strips_the_ticket_example() {
        // Hyphens riddle the alias and the delimiter is itself hyphenated, so the rsplit must
        // find the last `-dw-` and leave the base's own `-0731` tail intact.
        let index = aliases(&["deepseek-ai/DeepSeek-V4-Flash-0731"]);
        let (model, tokens) = resolve_model("deepseek-ai/DeepSeek-V4-Flash-0731-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "deepseek-ai/DeepSeek-V4-Flash-0731");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("flex")]);
    }

    #[test]
    fn multi_token_is_order_independent() {
        let index = aliases(&["m"]);
        let (a_model, mut a) = resolve_model("m-dw-flex-dw-cache", "-dw-", &index).unwrap();
        let (b_model, mut b) = resolve_model("m-dw-cache-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(a_model, "m");
        assert_eq!(b_model, "m");
        a.sort_by_key(|t| format!("{t:?}"));
        b.sort_by_key(|t| format!("{t:?}"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn unknown_base_is_never_rewritten() {
        // The stripped candidate isn't an alias → leave it alone and let onwards 404.
        let index = aliases(&["m"]);
        let (model, tokens) = resolve_model("other-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "other-dw-flex");
        assert!(tokens.is_empty());
    }

    #[test]
    fn empty_alias_table_rewrites_nothing() {
        // Mid-reload the map can be empty; behave exactly as today.
        let index = aliases(&[]);
        let (model, tokens) = resolve_model("m-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "m-dw-flex");
        assert!(tokens.is_empty());
    }

    #[test]
    fn delimiter_inside_the_base_is_preserved() {
        let index = aliases(&["a-dw-b"]);
        let (model, tokens) = resolve_model("a-dw-b-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "a-dw-b");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("flex")]);
    }

    #[test]
    fn colon_suffixes_are_inert_under_the_default_delimiter() {
        // Why `-dw-` is the default rather than `:`. Harnesses append their own colon tokens
        // for thinking strength (`:xhigh`), and Bedrock/Ollama ids contain colons outright.
        // None of that namespace is ours: the delimiter never matches, so each of these is
        // passed through untouched and routes exactly as it would with the feature off.
        let index = aliases(&["m", "anthropic.claude-v2:0", "llama3.1:8b"]);

        // The regression this delimiter exists to prevent: a live alias carrying a harness's
        // own colon token must not be read as a suffix and rejected with a 400.
        let (model, tokens) = resolve_model("m:xhigh", "-dw-", &index).unwrap();
        assert_eq!(model, "m:xhigh");
        assert!(tokens.is_empty());

        // Even a token that *is* one of ours stays inert behind a colon.
        let (model, tokens) = resolve_model("m:flex", "-dw-", &index).unwrap();
        assert_eq!(model, "m:flex");
        assert!(tokens.is_empty());

        // Colon-bearing ids still match exactly, as they always did...
        let (model, tokens) = resolve_model("anthropic.claude-v2:0", "-dw-", &index).unwrap();
        assert_eq!(model, "anthropic.claude-v2:0");
        assert!(tokens.is_empty());

        // ...and can still carry a real suffix.
        let (model, tokens) = resolve_model("llama3.1:8b-dw-flex", "-dw-", &index).unwrap();
        assert_eq!(model, "llama3.1:8b");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("flex")]);
    }

    #[test]
    fn contradictory_name_is_left_alone() {
        // Same param twice → not resolved to an arbitrary half; 404s as an unknown model.
        // The duplicate break happens on a Found token, so the UnknownSuffix 400 can't fire.
        let index = aliases(&["m"]);
        let (model, tokens) = resolve_model("m-dw-flex-dw-background", "-dw-", &index).unwrap();
        assert_eq!(model, "m-dw-flex-dw-background");
        assert!(tokens.is_empty());
    }

    #[test]
    fn colon_delimiter_still_works_when_configured() {
        // The delimiter is config, not a hard-coded literal: a deployment whose aliases
        // already contain `-dw-` can still override it, colon included.
        let index = aliases(&["zai/glm-5.2-fp8-dw"]);
        let (model, tokens) = resolve_model("zai/glm-5.2-fp8-dw:flex", ":", &index).unwrap();
        assert_eq!(model, "zai/glm-5.2-fp8-dw");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("flex")]);
    }

    #[test]
    fn explicit_name_value_suffix_form() {
        let index = aliases(&["m"]);
        let (model, tokens) = resolve_model("m-dw-serviceTier=priority", "-dw-", &index).unwrap();
        assert_eq!(model, "m");
        assert_eq!(tokens, vec![ParamToken::ServiceTier("priority")]);
    }

    #[test]
    fn bad_value_is_400_only_when_the_base_resolves() {
        let index = aliases(&["m"]);
        let error = resolve_model("m-dw-serviceTier=turbo", "-dw-", &index).unwrap_err();
        assert!(matches!(
            error,
            InvalidParam::Value {
                param: SERVICE_TIER_PARAM,
                source: Source::ModelSuffix,
                ..
            }
        ));

        // Unknown base → an ordinary unknown model, not a 400.
        let (model, tokens) = resolve_model("nope-dw-serviceTier=turbo", "-dw-", &index).unwrap();
        assert_eq!(model, "nope-dw-serviceTier=turbo");
        assert!(tokens.is_empty());
    }

    #[test]
    fn unknown_suffix_on_a_real_alias_is_400() {
        // The base resolved, so the delimiter was deliberate — say what's supported.
        let index = aliases(&["m"]);
        let error = resolve_model("m-dw-turbo", "-dw-", &index).unwrap_err();
        assert_eq!(
            error,
            InvalidParam::UnknownSuffix {
                token: "turbo".to_string()
            }
        );
    }

    #[test]
    fn unknown_suffix_on_an_unknown_base_is_not_400() {
        // A suffix aimed at a model this deployment doesn't host is a 404, not a 400.
        let index = aliases(&["m"]);
        let (model, tokens) = resolve_model("deepseek/deepseek-r1-dw-free", "-dw-", &index).unwrap();
        assert_eq!(model, "deepseek/deepseek-r1-dw-free");
        assert!(tokens.is_empty());
    }

    // ── parse: precedence ───────────────────────────────────────────────────

    #[test]
    fn body_beats_suffix_and_query() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m-dw-flex", "service_tier": "priority"});
        let params = parse(&body, Some("serviceTier=background"), &index, &cfg()).unwrap();
        assert_eq!(params.service_tier, Some(("priority", Source::Body)));
        // The model is still resolved — precedence is per-parameter.
        assert_eq!(params.model.as_deref(), Some("m"));
    }

    #[test]
    fn suffix_beats_query() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m-dw-flex"});
        let params = parse(&body, Some("serviceTier=background"), &index, &cfg()).unwrap();
        assert_eq!(params.service_tier, Some(("flex", Source::ModelSuffix)));
    }

    #[test]
    fn query_applies_when_nothing_else_does() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m"});
        let params = parse(&body, Some("serviceTier=flex"), &index, &cfg()).unwrap();
        assert_eq!(params.service_tier, Some(("flex", Source::Query)));
        assert_eq!(params.consumed_query_params, vec![SERVICE_TIER_PARAM]);
    }

    #[test]
    fn null_body_field_does_not_win() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m", "cache_control": null});
        let params = parse(&body, Some("cacheBreakpoint=lastUserMessage"), &index, &cfg()).unwrap();
        assert_eq!(params.cache_breakpoint, Some((CacheBreakpoint::LastUserMessage, Source::Query)));
    }

    #[test]
    fn first_query_occurrence_wins_and_unknown_names_are_ignored() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m"});
        let params = parse(&body, Some("serviceTier=flex&serviceTier=background&other=1"), &index, &cfg()).unwrap();
        assert_eq!(params.service_tier, Some(("flex", Source::Query)));

        // A differently-cased name is a different, ignored param.
        let params = parse(&body, Some("servicetier=flex"), &index, &cfg()).unwrap();
        assert_eq!(params.service_tier, None);
    }

    #[test]
    fn unknown_query_value_is_rejected() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m"});
        let error = parse(&body, Some("serviceTier=turbo"), &index, &cfg()).unwrap_err();
        assert!(matches!(
            error,
            InvalidParam::Value {
                param: SERVICE_TIER_PARAM,
                source: Source::Query,
                ..
            }
        ));
    }

    #[test]
    fn cache_breakpoint_unrecognised_when_cache_disabled() {
        // Must not be consumed or stripped — nothing downstream would remove the marker.
        let index = aliases(&["m"]);
        let body = json!({"model": "m"});
        let disabled = ParamsConfig {
            cache_breakpoint: false,
            ..cfg()
        };
        let params = parse(&body, Some("cacheBreakpoint=lastUserMessage"), &index, &disabled).unwrap();
        assert_eq!(params.cache_breakpoint, None);
        assert!(params.consumed_query_params.is_empty());
    }

    #[test]
    fn disabled_channels_are_inert() {
        let index = aliases(&["m"]);
        let body = json!({"model": "m-dw-flex"});
        let off = ParamsConfig {
            query_params: false,
            model_suffix: false,
            ..cfg()
        };
        let params = parse(&body, Some("serviceTier=flex"), &index, &off).unwrap();
        assert_eq!(
            params,
            RequestParams {
                requested_model: Some("m-dw-flex".into()),
                // Nothing applied, but the skew signal still fires — see below.
                suffix_ignored: true,
                ..Default::default()
            }
        );
    }

    // ── parse: multi-replica rollout skew ───────────────────────────────────

    #[test]
    fn disabled_channel_reports_an_ignored_suffix() {
        // The signal that this pod is behind a partial rollout: the request would have
        // resolved, so it is succeeding on whichever replicas have the flag on.
        let index = aliases(&["m"]);
        let off = ParamsConfig {
            model_suffix: false,
            ..cfg()
        };
        let params = parse(&json!({"model": "m-dw-flex"}), None, &index, &off).unwrap();
        assert!(params.suffix_ignored);
        // ...and nothing was actually applied.
        assert_eq!(params.model, None);
        assert_eq!(params.service_tier, None);
    }

    #[test]
    fn disabled_channel_does_not_false_positive_on_a_colon_alias() {
        // A Bedrock id is not a suffixed request; exact match returns before any split.
        let index = aliases(&["anthropic.claude-v2:0"]);
        let off = ParamsConfig {
            model_suffix: false,
            ..cfg()
        };
        let params = parse(&json!({"model": "anthropic.claude-v2:0"}), None, &index, &off).unwrap();
        assert!(!params.suffix_ignored);
    }

    #[test]
    fn disabled_channel_never_errors() {
        // A disabled channel must not reject anything, so the UnknownSuffix 400 cannot fire
        // on a pod where the feature is off — otherwise a partial rollout would turn a 404
        // into a 400 depending on which replica answered.
        let index = aliases(&["m"]);
        let off = ParamsConfig {
            model_suffix: false,
            ..cfg()
        };
        let params = parse(&json!({"model": "m-dw-turbo"}), None, &index, &off).unwrap();
        assert!(!params.suffix_ignored);
    }

    #[test]
    fn missing_or_non_string_model_does_not_panic() {
        let index = aliases(&["m"]);
        for body in [json!({}), json!({"model": 42}), json!({"model": null}), json!("not an object")] {
            let params = parse(&body, None, &index, &cfg()).unwrap();
            assert_eq!(params.model, None);
        }
    }

    // ── apply_to_body ───────────────────────────────────────────────────────

    #[test]
    fn apply_returns_false_when_nothing_resolved() {
        // Guards the byte-identical default path: no change → no re-serialise.
        let params = RequestParams {
            requested_model: Some("m".into()),
            ..Default::default()
        };
        let mut body = json!({"model": "m"});
        assert!(!apply_to_body(&params, &mut body));
        assert_eq!(body, json!({"model": "m"}));
    }

    #[test]
    fn apply_writes_stripped_model_and_tier() {
        let index = aliases(&["m"]);
        let mut body = json!({"model": "m-dw-flex", "messages": []});
        let params = parse(&body.clone(), None, &index, &cfg()).unwrap();
        assert!(apply_to_body(&params, &mut body));
        assert_eq!(body["model"], "m");
        assert_eq!(body["service_tier"], "flex");
    }

    #[test]
    fn apply_does_not_overwrite_explicit_cache_control() {
        let index = aliases(&["m"]);
        let mut body = json!({"model": "m", "cache_control": {"type": "ephemeral", "ttl": "5m"}});
        let params = parse(&body.clone(), Some("cacheBreakpoint=lastUserMessage"), &index, &cfg()).unwrap();
        // Body wins, so there is nothing out-of-band left to write.
        assert!(!apply_to_body(&params, &mut body));
        assert_eq!(body["cache_control"]["ttl"], "5m");
    }

    #[test]
    fn apply_injects_marker_from_query() {
        let index = aliases(&["m"]);
        let mut body = json!({"model": "m"});
        let params = parse(&body.clone(), Some("cacheBreakpoint=lastUserMessage"), &index, &cfg()).unwrap();
        assert!(apply_to_body(&params, &mut body));
        assert_eq!(body["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    }

    #[test]
    fn apply_no_ops_on_non_object_body() {
        let params = RequestParams {
            model: Some("m".into()),
            requested_model: Some("m-dw-flex".into()),
            ..Default::default()
        };
        let mut body = json!(["not", "an", "object"]);
        assert!(!apply_to_body(&params, &mut body));
    }

    // ── strip_params ────────────────────────────────────────────────────────

    #[test]
    fn strip_removes_named_params_and_preserves_others() {
        let uri: Uri = "/v1/chat/completions?foo=bar&serviceTier=flex&baz=1".parse().unwrap();
        assert_eq!(
            strip_params(&uri, &[SERVICE_TIER_PARAM]).to_string(),
            "/v1/chat/completions?foo=bar&baz=1"
        );

        // Several at once, and duplicates.
        let uri: Uri = "/v1/chat/completions?serviceTier=flex&x=y&cacheBreakpoint=lastUserMessage&serviceTier=a"
            .parse()
            .unwrap();
        assert_eq!(
            strip_params(&uri, &[SERVICE_TIER_PARAM, CACHE_BREAKPOINT_PARAM]).to_string(),
            "/v1/chat/completions?x=y"
        );

        // Only ours → the whole query goes, no trailing '?'.
        let uri: Uri = "/v1/chat/completions?serviceTier=flex".parse().unwrap();
        assert_eq!(strip_params(&uri, &[SERVICE_TIER_PARAM]).to_string(), "/v1/chat/completions");

        // Absent, empty name list, and no query at all → unchanged.
        let uri: Uri = "/v1/chat/completions?foo=bar".parse().unwrap();
        assert_eq!(
            strip_params(&uri, &[SERVICE_TIER_PARAM]).to_string(),
            "/v1/chat/completions?foo=bar"
        );
        assert_eq!(strip_params(&uri, &[]).to_string(), "/v1/chat/completions?foo=bar");
        let uri: Uri = "/v1/chat/completions".parse().unwrap();
        assert_eq!(strip_params(&uri, &[SERVICE_TIER_PARAM]).to_string(), "/v1/chat/completions");
    }
}
