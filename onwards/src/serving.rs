//! Serving classes: the per-request dispatch mode and how it is resolved.
//!
//! A serving class names how a request wants to be served. `interactive`
//! (per-stream speed, fast first token) and `throughput` (aggregate volume)
//! are *elevated* classes a model activates and an organisation is granted;
//! `standard` is what everyone gets when nothing is set. In v1 the class maps
//! directly onto the serving stack's two interactivity pools and a priority
//! band: an elevated class is stamped on the dynamo member as a pool tag plus
//! priority, `standard` carries nothing and is byte-identical to today.
//!
//! Resolution is one flat field, never a chain the caller has to reason
//! about:
//!
//! ```text
//! requested = model suffix, else the key's class, else the overlay's default
//!             for this model, else the account's default, else none
//! resolved  = requested if the model has it active AND the org's overlay
//!             grants it, else standard
//! ```
//!
//! Daemon legs (batch and flex on the batch-purpose key, continuation resume
//! legs) always resolve to `standard`: their existing deadline priorities
//! carry the ordering and no tag is sent.
//!
//! Everything a client could send to steer this itself — the pool tag, the
//! priority header, the `nvext` pool field — is stripped at ingress; only the
//! resolver sets them.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Header carrying the pool selection to the dynamo frontend.
pub const POOL_TAG_HEADER: &str = "x-dynamo-interactivity-pool";
/// Header aliasing `nvext.agent_hints.priority` on the dynamo frontend.
pub const PRIORITY_HEADER: &str = "x-dynamo-request-priority";
/// The typed body-side twin of [`POOL_TAG_HEADER`] (`nvext.interactivity_pool`).
pub const NVEXT_POOL_FIELD: &str = "interactivity_pool";
/// Separator between a model alias and its class suffix (`alias:class`).
pub const SUFFIX_SEPARATOR: char = ':';

/// The dispatch mode a request is served under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServingClass {
    /// Elevated: the high-interactivity (KV-budgeted) pool, band 200.
    Interactive,
    /// Elevated: the default pool with precedence over standard, band 100.
    Throughput,
    /// The absence of a choice: default pool, band 0, nothing stamped.
    Standard,
}

impl ServingClass {
    /// Every class, in the order the 400 message lists them.
    pub const ALL: [ServingClass; 3] = [
        ServingClass::Interactive,
        ServingClass::Throughput,
        ServingClass::Standard,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ServingClass::Interactive => "interactive",
            ServingClass::Throughput => "throughput",
            ServingClass::Standard => "standard",
        }
    }

    /// Elevated classes are the ones a model activates and an org is granted.
    pub fn is_elevated(self) -> bool {
        !matches!(self, ServingClass::Standard)
    }

    /// The pool tag sent to the dynamo frontend; `None` for `standard`, which
    /// travels untagged so it lands in dynamo's default pool exactly as today.
    pub fn pool_tag(self) -> Option<&'static str> {
        match self {
            ServingClass::Interactive => Some("interactive"),
            ServingClass::Throughput => Some("throughput"),
            ServingClass::Standard => None,
        }
    }

    /// Realtime priority band. Higher wins on the dynamo frontend; `standard`
    /// is 0, today's implicit realtime value.
    pub fn priority_band(self) -> i32 {
        match self {
            ServingClass::Interactive => 200,
            ServingClass::Throughput => 100,
            ServingClass::Standard => 0,
        }
    }
}

impl fmt::Display for ServingClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A class name that is not in the fixed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownServingClass(pub String);

impl fmt::Display for UnknownServingClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Unknown serving class '{}'. Valid classes: {}.",
            self.0,
            ServingClass::ALL
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for UnknownServingClass {}

impl FromStr for ServingClass {
    type Err = UnknownServingClass;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ServingClass::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownServingClass(s.to_string()))
    }
}

/// Split `alias:class` into the bare alias and the requested class.
///
/// A model string without a separator is returned unchanged. Aliases never
/// contain `:` today, so anything after the last one is a class request and
/// an unknown class is an error, never silently ignored: a typo must not
/// quietly change how a request is served.
pub fn split_class_suffix(model: &str) -> Result<(&str, Option<ServingClass>), UnknownServingClass> {
    match model.rsplit_once(SUFFIX_SEPARATOR) {
        None => Ok((model, None)),
        Some((alias, suffix)) => suffix.parse().map(|class| (alias, Some(class))),
    }
}

/// Request extension: the class the request asked for by suffix.
///
/// Inserted by whoever stripped the suffix ahead of onwards (dwctl's inference
/// middleware does, so the model string every layer above onwards keys on —
/// analytics, the prompt cache, billing — is the bare alias). Onwards also
/// parses a suffix it still finds, so the extension is an optimisation, not a
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedServingClass(pub ServingClass);

/// Response extension: what the request asked for and what it was served as.
///
/// Present on every response that went through the resolver (which is every
/// response an upstream produced); absent on auth/validation rejections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingClassOutcome {
    pub requested: Option<ServingClass>,
    pub resolved: ServingClass,
}

/// One organisation's modifiers on one model (an *overlay*).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingOverlay {
    /// Elevated classes the org may use on this model.
    #[serde(default)]
    pub granted: Vec<ServingClass>,
    /// Class the org's requests to this model ask for when neither the request
    /// nor the key names one. Outranks the account-wide default.
    #[serde(default)]
    pub default_class: Option<ServingClass>,
    /// Per-model override of the account's `self_hosted_only`.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
}

/// The serving policy attached to an API key: the key's own class, the
/// owning account's settings, and the account's overlays by model alias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyServing {
    /// Class this key requests by default.
    #[serde(default)]
    pub class: Option<ServingClass>,
    /// Account setting: class the account's requests ask for by default.
    #[serde(default)]
    pub default_class: Option<ServingClass>,
    /// Account setting: never fall over to an external (untrusted) provider.
    #[serde(default)]
    pub self_hosted_only: bool,
    /// Overlays by model alias.
    #[serde(default)]
    pub overlays: HashMap<String, ServingOverlay>,
}

impl KeyServing {
    /// True when nothing here changes a request: the sync omits such policies
    /// so unaffected keys' config stays byte-identical.
    pub fn is_empty(&self) -> bool {
        self.class.is_none() && self.default_class.is_none() && !self.self_hosted_only && self.overlays.is_empty()
    }
}

/// Key purposes whose requests are daemon legs: the ordering already travels
/// as a deadline-derived (batch, flex) or fixed (continuation resume)
/// priority in the body, and no class is sent.
const DAEMON_PURPOSES: [&str; 2] = ["batch", "continuation"];

/// The outcome of resolving one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingResolution {
    pub requested: Option<ServingClass>,
    pub resolved: ServingClass,
    /// Restrict the composite to its self-hosted (trusted) members.
    pub self_hosted_only: bool,
}

impl ServingResolution {
    pub fn outcome(&self) -> ServingClassOutcome {
        ServingClassOutcome {
            requested: self.requested,
            resolved: self.resolved,
        }
    }
}

/// Resolve the serving class for one request.
///
/// * `suffix` — the class named on the request itself, if any.
/// * `key` — the serving policy of the authenticated key, if it has one.
/// * `alias` — the model alias the customer called (the overlay key).
/// * `active` — the elevated classes the model has activated.
/// * `purpose` — the key's purpose label; daemon purposes resolve to standard.
pub fn resolve(
    suffix: Option<ServingClass>,
    key: Option<&KeyServing>,
    alias: &str,
    active: &[ServingClass],
    purpose: Option<&str>,
) -> ServingResolution {
    let overlay = key.and_then(|k| k.overlays.get(alias));
    let self_hosted_only = overlay
        .and_then(|o| o.self_hosted_only)
        .unwrap_or_else(|| key.is_some_and(|k| k.self_hosted_only));

    if purpose.is_some_and(|p| DAEMON_PURPOSES.contains(&p)) {
        return ServingResolution {
            requested: None,
            resolved: ServingClass::Standard,
            self_hosted_only,
        };
    }

    let requested = suffix
        .or(key.and_then(|k| k.class))
        .or(overlay.and_then(|o| o.default_class))
        .or(key.and_then(|k| k.default_class));

    let resolved = match requested {
        Some(class) if class.is_elevated() => {
            let granted = overlay.is_some_and(|o| o.granted.contains(&class));
            if active.contains(&class) && granted {
                class
            } else {
                ServingClass::Standard
            }
        }
        _ => ServingClass::Standard,
    };

    ServingResolution {
        requested,
        resolved,
        self_hosted_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(class: Option<ServingClass>, default: Option<ServingClass>, overlays: &[(&str, ServingOverlay)]) -> KeyServing {
        KeyServing {
            class,
            default_class: default,
            self_hosted_only: false,
            overlays: overlays.iter().map(|(a, o)| (a.to_string(), o.clone())).collect(),
        }
    }

    fn grant(classes: &[ServingClass]) -> ServingOverlay {
        ServingOverlay {
            granted: classes.to_vec(),
            ..Default::default()
        }
    }

    const BOTH: [ServingClass; 2] = [ServingClass::Interactive, ServingClass::Throughput];

    #[test]
    fn suffix_is_split_and_unknown_class_is_an_error() {
        assert_eq!(split_class_suffix("zai-org/GLM-5.2"), Ok(("zai-org/GLM-5.2", None)));
        assert_eq!(
            split_class_suffix("zai-org/GLM-5.2:interactive"),
            Ok(("zai-org/GLM-5.2", Some(ServingClass::Interactive)))
        );
        assert_eq!(
            split_class_suffix("m:standard"),
            Ok(("m", Some(ServingClass::Standard)))
        );
        let err = split_class_suffix("m:fast").unwrap_err();
        assert_eq!(err, UnknownServingClass("fast".to_string()));
        assert!(err.to_string().contains("interactive, throughput, standard"));
    }

    #[test]
    fn nothing_set_resolves_to_standard_with_no_request() {
        let r = resolve(None, None, "m", &BOTH, Some("realtime"));
        assert_eq!(r.requested, None);
        assert_eq!(r.resolved, ServingClass::Standard);
        assert!(!r.self_hosted_only);
    }

    #[test]
    fn precedence_is_suffix_then_key_then_overlay_then_account() {
        let k = key(
            Some(ServingClass::Throughput),
            Some(ServingClass::Interactive),
            &[("m", grant(&BOTH))],
        );
        // suffix wins over the key
        let r = resolve(Some(ServingClass::Interactive), Some(&k), "m", &BOTH, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Interactive);
        // key wins over the account default
        let r = resolve(None, Some(&k), "m", &BOTH, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Throughput);
        // overlay default wins over the account default
        let k2 = key(
            None,
            Some(ServingClass::Interactive),
            &[(
                "m",
                ServingOverlay {
                    granted: BOTH.to_vec(),
                    default_class: Some(ServingClass::Throughput),
                    self_hosted_only: None,
                },
            )],
        );
        let r = resolve(None, Some(&k2), "m", &BOTH, Some("realtime"));
        assert_eq!(r.requested, Some(ServingClass::Throughput));
        assert_eq!(r.resolved, ServingClass::Throughput);
        // account default alone
        let k3 = key(None, Some(ServingClass::Interactive), &[("m", grant(&BOTH))]);
        let r = resolve(None, Some(&k3), "m", &BOTH, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn explicit_standard_suffix_opts_down_and_is_recorded() {
        let k = key(Some(ServingClass::Interactive), None, &[("m", grant(&BOTH))]);
        let r = resolve(Some(ServingClass::Standard), Some(&k), "m", &BOTH, Some("realtime"));
        assert_eq!(r.requested, Some(ServingClass::Standard));
        assert_eq!(r.resolved, ServingClass::Standard);
    }

    #[test]
    fn unentitled_or_inactive_requests_resolve_to_standard_but_stay_visible() {
        // granted but the model has not activated the class
        let k = key(Some(ServingClass::Interactive), None, &[("m", grant(&BOTH))]);
        let r = resolve(None, Some(&k), "m", &[], Some("realtime"));
        assert_eq!(r.requested, Some(ServingClass::Interactive));
        assert_eq!(r.resolved, ServingClass::Standard);
        // active but not granted (no overlay for this model)
        let k = key(Some(ServingClass::Interactive), None, &[("other", grant(&BOTH))]);
        let r = resolve(None, Some(&k), "m", &BOTH, Some("realtime"));
        assert_eq!(r.requested, Some(ServingClass::Interactive));
        assert_eq!(r.resolved, ServingClass::Standard);
        // active, overlay exists, but grants only the other class
        let k = key(Some(ServingClass::Interactive), None, &[("m", grant(&[ServingClass::Throughput]))]);
        let r = resolve(None, Some(&k), "m", &BOTH, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Standard);
    }

    #[test]
    fn daemon_legs_always_resolve_to_standard() {
        let k = key(Some(ServingClass::Interactive), None, &[("m", grant(&BOTH))]);
        for purpose in DAEMON_PURPOSES {
            let r = resolve(Some(ServingClass::Interactive), Some(&k), "m", &BOTH, Some(purpose));
            assert_eq!(r.requested, None, "{purpose}");
            assert_eq!(r.resolved, ServingClass::Standard, "{purpose}");
        }
        // playground behaves like realtime
        let r = resolve(None, Some(&k), "m", &BOTH, Some("playground"));
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn self_hosted_only_comes_from_the_account_unless_the_overlay_overrides() {
        let mut k = key(None, None, &[("m", grant(&[]))]);
        k.self_hosted_only = true;
        assert!(resolve(None, Some(&k), "m", &[], Some("realtime")).self_hosted_only);
        assert!(resolve(None, Some(&k), "other", &[], Some("realtime")).self_hosted_only);
        // the daemon leg carries the restriction too: batch work must not spill either
        assert!(resolve(None, Some(&k), "m", &[], Some("batch")).self_hosted_only);
        k.overlays.get_mut("m").unwrap().self_hosted_only = Some(false);
        assert!(!resolve(None, Some(&k), "m", &[], Some("realtime")).self_hosted_only);
        assert!(resolve(None, Some(&k), "other", &[], Some("realtime")).self_hosted_only);
    }

    #[test]
    fn bands_and_tags() {
        assert_eq!(ServingClass::Interactive.pool_tag(), Some("interactive"));
        assert_eq!(ServingClass::Throughput.pool_tag(), Some("throughput"));
        assert_eq!(ServingClass::Standard.pool_tag(), None);
        assert_eq!(ServingClass::Interactive.priority_band(), 200);
        assert_eq!(ServingClass::Throughput.priority_band(), 100);
        assert_eq!(ServingClass::Standard.priority_band(), 0);
        assert!(KeyServing::default().is_empty());
    }

    #[test]
    fn serde_uses_lowercase_names() {
        let json = serde_json::to_string(&KeyServing {
            class: Some(ServingClass::Interactive),
            ..Default::default()
        })
        .unwrap();
        assert!(json.contains("\"class\":\"interactive\""));
        let back: KeyServing = serde_json::from_str(&json).unwrap();
        assert_eq!(back.class, Some(ServingClass::Interactive));
    }
}

/// End-to-end behaviour through the request handler: what reaches an
/// upstream member for each resolution outcome.
#[cfg(test)]
mod handler_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;

    use super::*;
    use crate::load_balancer::{Provider, ProviderPool};
    use crate::target::{FallbackConfig, LoadBalanceStrategy, Target, TargetPools, Targets};
    use crate::test_utils::{MockHttpClient, MockRequest};
    use crate::{AppState, build_router};

    const KEY: &str = "sk-serving-test";
    const ALIAS: &str = "gpt-4";
    const OK_BODY: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4","choices":[]}"#;

    /// A member of the composite: `(url, trusted/self-hosted, accepts scheduling fields)`.
    type Member = (&'static str, bool, bool);

    fn targets(members: &[Member], active: &[ServingClass], purpose: &str, policy: Option<KeyServing>) -> Targets {
        let providers = members
            .iter()
            .map(|(url, trusted, accepts)| {
                let t = Target::builder()
                    .url(url.parse().unwrap())
                    .trusted(*trusted)
                    .accepts_scheduling_priority(*accepts)
                    .build();
                Provider::new(t, 1)
            })
            .collect();
        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            Some(FallbackConfig {
                enabled: true,
                on_status: vec![500],
                ..Default::default()
            }),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        )
        .with_serving_classes(active.to_vec());
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(ALIAS.to_string(), TargetPools::with_pools(pool, HashMap::new()));
        let key_labels = Arc::new(DashMap::new());
        key_labels.insert(KEY.to_string(), HashMap::from([("purpose".to_string(), purpose.to_string())]));
        let key_serving = Arc::new(DashMap::new());
        if let Some(policy) = policy {
            key_serving.insert(KEY.to_string(), policy);
        }
        Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels,
            key_serving,
            strict_mode: false,
            http_pool_config: None,
        }
    }

    fn granted(classes: &[ServingClass]) -> KeyServing {
        KeyServing {
            overlays: HashMap::from([(
                ALIAS.to_string(),
                ServingOverlay {
                    granted: classes.to_vec(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    fn header<'a>(req: &'a MockRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn body(req: &MockRequest) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap()
    }

    async fn post(server: &TestServer, model: &str, extra_headers: &[(&str, &str)]) -> axum_test::TestResponse {
        let mut req = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {KEY}"))
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}));
        for (k, v) in extra_headers {
            req = req.add_header(*k, *v);
        }
        req.await
    }

    const BOTH: [ServingClass; 2] = [ServingClass::Interactive, ServingClass::Throughput];

    #[tokio::test]
    async fn elevated_class_is_stamped_on_an_accepting_member_and_the_suffix_is_stripped() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();

        let response = post(&server, "gpt-4:interactive", &[]).await;
        assert_eq!(response.status_code(), 200);

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("interactive"));
        assert_eq!(header(req, PRIORITY_HEADER), Some("200"));
        assert_eq!(body(req)["model"], "gpt-4", "the suffix never reaches an upstream");
        assert_eq!(body(req)["messages"][0]["content"], "hi", "the rest of the body is untouched");
    }

    #[tokio::test]
    async fn throughput_carries_its_own_tag_and_band() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4:throughput", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("throughput"));
        assert_eq!(header(req, PRIORITY_HEADER), Some("100"));
    }

    #[tokio::test]
    async fn a_member_that_does_not_accept_scheduling_fields_sees_no_envelope() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://third-party.example.com/", false, false)], &BOTH, "realtime", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4:interactive", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(header(req, PRIORITY_HEADER), None);
        assert_eq!(body(req)["model"], "gpt-4");
    }

    #[tokio::test]
    async fn standard_and_unentitled_requests_travel_untagged() {
        // No policy at all: byte-identical to today.
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", None),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(header(req, PRIORITY_HEADER), None);

        // A suffix the org is not granted resolves to standard: served, untagged.
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", None),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4:interactive", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(body(req)["model"], "gpt-4");

        // Granted but the model has not activated the class: same outcome.
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &[], "realtime", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4:interactive", &[]).await.status_code(), 200);
        assert_eq!(header(&mock.get_requests()[0], POOL_TAG_HEADER), None);
    }

    /// The envelope is forwarded, not filtered: this crate is also the hop
    /// inside the serving namespace, which has no key policy and must pass on
    /// what the first hop stamped. Client-supplied values are removed at
    /// dwctl's ingress instead. When this hop does resolve an elevated class,
    /// its own values win.
    #[tokio::test]
    async fn inbound_envelope_headers_pass_through_and_a_resolution_overwrites_them() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", None),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        let response = post(
            &server,
            "gpt-4",
            &[(POOL_TAG_HEADER, "interactive"), (PRIORITY_HEADER, "200")],
        )
        .await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("interactive"), "second-hop pass-through");
        assert_eq!(header(req, PRIORITY_HEADER), Some("200"));

        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        let response = post(
            &server,
            "gpt-4:throughput",
            &[(POOL_TAG_HEADER, "interactive"), (PRIORITY_HEADER, "999")],
        )
        .await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("throughput"), "the resolution wins");
        assert_eq!(header(req, PRIORITY_HEADER), Some("100"));
    }

    #[tokio::test]
    async fn daemon_purpose_keys_resolve_to_standard_even_with_a_suffix() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "batch", Some(granted(&BOTH))),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        assert_eq!(post(&server, "gpt-4:interactive", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(header(req, PRIORITY_HEADER), None);
        assert_eq!(body(req)["model"], "gpt-4");
    }

    #[tokio::test]
    async fn unknown_class_suffix_is_rejected_with_the_valid_set() {
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let state = AppState::with_client(
            targets(&[("https://dynamo.example.com/", true, true)], &BOTH, "realtime", None),
            mock.clone(),
        );
        let server = TestServer::new(build_router(state)).unwrap();
        let response = post(&server, "gpt-4:fast", &[]).await;
        assert_eq!(response.status_code(), 400);
        let err: serde_json::Value = response.json();
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("'fast'"), "{message}");
        assert!(message.contains("interactive, throughput, standard"), "{message}");
        assert!(mock.get_requests().is_empty(), "nothing reaches an upstream");
    }

    #[tokio::test]
    async fn self_hosted_only_accounts_never_reach_an_external_member() {
        let members: [Member; 2] = [
            ("https://dynamo.example.com/", true, true),
            ("https://third-party.example.com/", false, false),
        ];
        // The self-hosted member fails; the pool would normally fall over to
        // the external one.
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let state = AppState::with_client(targets(&members, &[], "realtime", None), mock.clone());
        let server = TestServer::new(build_router(state)).unwrap();
        post(&server, "gpt-4", &[]).await;
        assert_eq!(mock.get_requests().len(), 2, "control: without the setting both members are tried");

        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let policy = KeyServing {
            self_hosted_only: true,
            ..Default::default()
        };
        let state = AppState::with_client(targets(&members, &[], "realtime", Some(policy)), mock.clone());
        let server = TestServer::new(build_router(state)).unwrap();
        let response = post(&server, "gpt-4", &[]).await;
        assert!(response.status_code().is_server_error(), "{}", response.status_code());
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 1, "the external member is never attempted");
        assert!(requests[0].uri.starts_with("https://dynamo.example.com/"));

        // A per-model overlay can lift the account-wide restriction.
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let policy = KeyServing {
            self_hosted_only: true,
            overlays: HashMap::from([(
                ALIAS.to_string(),
                ServingOverlay {
                    self_hosted_only: Some(false),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let state = AppState::with_client(targets(&members, &[], "realtime", Some(policy)), mock.clone());
        let server = TestServer::new(build_router(state)).unwrap();
        post(&server, "gpt-4", &[]).await;
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[test]
    fn key_serving_round_trips_through_config_json() {
        let json = json!({
            "key": "sk-1",
            "labels": {"purpose": "realtime"},
            "serving": {
                "class": "interactive",
                "self_hosted_only": true,
                "overlays": {"gpt-4": {"granted": ["interactive", "throughput"], "default_class": "throughput"}}
            }
        });
        let def: crate::target::KeyDefinition = serde_json::from_value(json).unwrap();
        let serving = def.serving.unwrap();
        assert_eq!(serving.class, Some(ServingClass::Interactive));
        assert!(serving.self_hosted_only);
        assert_eq!(serving.overlays["gpt-4"].default_class, Some(ServingClass::Throughput));
        // Absent: an ordinary key definition still parses.
        let def: crate::target::KeyDefinition = serde_json::from_value(json!({"key": "sk-2"})).unwrap();
        assert!(def.serving.is_none());
    }
}
