//! The `cacheBreakpoint` query parameter — automatic caching for clients that can't touch the body.
//!
//! Some integrators (proxy layers forwarding their own customers' OpenAI-shaped traffic) can't
//! inject fields into request bodies, so they can't opt into automatic caching via the top-level
//! `cache_control` marker. `?cacheBreakpoint=lastUserMessage` on `/chat/completions` is the
//! out-of-band equivalent: the cache layer translates it into the top-level marker (1h tier)
//! before anything else looks at the body, so validation, classification, and outbound marker
//! stripping all see exactly what they'd see had the client sent the field itself — one code path.
//!
//! Rules:
//! - The param name and values are matched byte-exact — no percent-decoding is performed, so
//!   encoded forms (e.g. `last%55serMessage`) are treated as distinct and therefore unsupported.
//!   No real client percent-encodes unreserved ASCII unprompted; an encoded value gets the
//!   strict 400 below, an encoded param NAME is simply an unknown (ignored) param.
//! - An unrecognized value is a 400 ([`layer`](super::layer) builds the response): a typo must
//!   not silently disable caching while the caller believes it's on.
//! - A non-null top-level `cache_control` already in the body wins and the param is ignored —
//!   an end client that explicitly opted in *through* a param-appending proxy keeps its explicit
//!   behavior. (A `null` body field is "no marker", per `parse`, so the param applies.)
//! - The param is stripped from the forwarded URI: onwards forwards `path_and_query` verbatim
//!   to providers, and the param must not leak upstream.
//! - Value → tier: `lastUserMessage` → `1h`. The tier is explicit in the injected marker (not
//!   the policy default) per the product decision; a deployment that disables the 1h tier will
//!   reject param-carrying requests at marker validation, like any other 1h marker.
//!
//! Requests on non-cacheable routes (e.g. `/v1/embeddings`) bypass the cache layer entirely, so
//! the param passes through to the provider there — harmless (OpenAI-compatible servers ignore
//! unknown query params), but only `/chat/completions` honours it.

use serde_json::{Value, json};

/// The query parameter name, on `/chat/completions` only.
pub const CACHE_BREAKPOINT_PARAM: &str = "cacheBreakpoint";

/// The one supported value: mark the last cacheable block of the request (Anthropic automatic-
/// caching semantics — for turn-based chat that's the latest user message; for a request ending
/// in tool results it's the last tool block, i.e. a strictly larger prefix).
pub(crate) const LAST_USER_MESSAGE: &str = "lastUserMessage";

/// The marker injected for [`LAST_USER_MESSAGE`]. Explicit `1h` tier (not the policy default).
pub(crate) fn last_user_message_marker() -> Value {
    json!({"type": "ephemeral", "ttl": "1h"})
}

/// The param was present but its value isn't supported (the caller turns this into a 400).
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidBreakpointValue(pub String);

/// Extract the `cacheBreakpoint` marker from a raw query string. `Ok(None)` when the param is
/// absent; `Ok(Some(marker))` for a supported value; `Err` otherwise (including an empty or
/// missing value — strictness is the point). First occurrence wins if duplicated.
pub fn breakpoint_marker(query: Option<&str>) -> Result<Option<Value>, InvalidBreakpointValue> {
    let Some(query) = query else { return Ok(None) };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != CACHE_BREAKPOINT_PARAM {
            continue;
        }
        return match value {
            LAST_USER_MESSAGE => Ok(Some(last_user_message_marker())),
            other => Err(InvalidBreakpointValue(other.to_string())),
        };
    }
    Ok(None)
}

/// What [`inject_marker`] did with the body.
#[derive(Debug, PartialEq, Eq)]
pub enum Inject {
    /// The marker was inserted as the body's top-level `cache_control`.
    Applied,
    /// The body already carries a non-null top-level `cache_control` — explicit wins, param ignored.
    BodyFieldWins,
    /// The body isn't a JSON object (onwards will reject it downstream); nothing to inject into.
    NotAnObject,
}

/// Insert the marker as the body's top-level `cache_control`, unless an explicit non-null one is
/// already there. An explicit `null` is "no marker" (matching `parse`) and is overwritten.
pub fn inject_marker(body: &mut Value, marker: Value) -> Inject {
    let Some(obj) = body.as_object_mut() else {
        return Inject::NotAnObject;
    };
    match obj.get("cache_control") {
        Some(cc) if !cc.is_null() => Inject::BodyFieldWins,
        _ => {
            obj.insert("cache_control".into(), marker);
            Inject::Applied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_param_is_none() {
        assert_eq!(breakpoint_marker(None), Ok(None));
        assert_eq!(breakpoint_marker(Some("")), Ok(None));
        assert_eq!(breakpoint_marker(Some("foo=bar&stream=true")), Ok(None));
    }

    #[test]
    fn supported_value_yields_1h_marker() {
        let marker = breakpoint_marker(Some("cacheBreakpoint=lastUserMessage")).unwrap().unwrap();
        assert_eq!(marker, json!({"type": "ephemeral", "ttl": "1h"}));
        // Position and neighbours don't matter.
        let marker = breakpoint_marker(Some("a=b&cacheBreakpoint=lastUserMessage&c=d")).unwrap().unwrap();
        assert_eq!(marker, json!({"type": "ephemeral", "ttl": "1h"}));
    }

    #[test]
    fn unknown_or_empty_value_is_invalid() {
        // Strict: a typo must be a 400, not a silent no-cache.
        assert_eq!(
            breakpoint_marker(Some("cacheBreakpoint=lastusermessage")),
            Err(InvalidBreakpointValue("lastusermessage".into()))
        );
        assert_eq!(breakpoint_marker(Some("cacheBreakpoint=")), Err(InvalidBreakpointValue("".into())));
        assert_eq!(breakpoint_marker(Some("cacheBreakpoint")), Err(InvalidBreakpointValue("".into())));
    }

    #[test]
    fn first_occurrence_wins_and_name_is_case_sensitive() {
        assert_eq!(
            breakpoint_marker(Some("cacheBreakpoint=bogus&cacheBreakpoint=lastUserMessage")),
            Err(InvalidBreakpointValue("bogus".into()))
        );
        // Different casing is a different (ignored) param, per query-string convention.
        assert_eq!(breakpoint_marker(Some("cachebreakpoint=lastUserMessage")), Ok(None));
    }

    #[test]
    fn inject_applies_and_explicit_body_field_wins() {
        let mut body = json!({"model": "m", "messages": []});
        assert_eq!(inject_marker(&mut body, last_user_message_marker()), Inject::Applied);
        assert_eq!(body["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));

        // Non-null body field wins, untouched.
        let mut body = json!({"model": "m", "cache_control": {"type": "ephemeral", "ttl": "5m"}});
        assert_eq!(inject_marker(&mut body, last_user_message_marker()), Inject::BodyFieldWins);
        assert_eq!(body["cache_control"]["ttl"], "5m");

        // A null body field is "no marker" (parse semantics) → the param applies.
        let mut body = json!({"model": "m", "cache_control": null});
        assert_eq!(inject_marker(&mut body, last_user_message_marker()), Inject::Applied);
        assert_eq!(body["cache_control"]["ttl"], "1h");

        // Non-object bodies have nowhere to inject.
        let mut body = json!(["not", "an", "object"]);
        assert_eq!(inject_marker(&mut body, last_user_message_marker()), Inject::NotAnObject);
    }
}
