//! Per-request serving policy: scheduling hints and the self-hosted
//! restriction.
//!
//! A request's **serving class** is where it is routed: today every request
//! resolves as [`ServingClass::Standard`]. A class is a destination on the
//! model, never a customer setting.
//!
//! On top of the class, an account may carry **scheduling hints**: a priority
//! (account-wide, or per alias) and per-alias time-to-first-token and
//! inter-token-latency targets, from an organisation overlay or the account
//! catalog. They are sent to members of kind `dynamo` but change neither the
//! class nor the price. The targets are labels the serving stack may use for
//! alerting and diagnostics; nothing routes on them.
//!
//! Daemon legs (batch and flex on the batch-purpose key, continuation resume
//! legs) never receive hints and keep their own priorities: batch and flex
//! carry deadline-derived priorities, background dispatch the reserved
//! `i32::MIN`, and continuation resumes their fixed priority.
//!
//! The targets travel in the request body as `nvext.router.{ttft_target,
//! itl_target}` and the priority as `nvext.agent_hints.priority`, on members
//! of kind `dynamo` only. A request to a `dynamo` member that carries no
//! priority is sent [`DEFAULT_PRIORITY`], so every priority is relative to
//! realtime's 0. Everything a client could send to steer this itself is
//! scrubbed at dwctl's ingress; only the resolver sets them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The vendor extension object on a request body (`nvext`).
pub const NVEXT_FIELD: &str = "nvext";
/// The router parameters inside `nvext` (`nvext.router`).
pub const NVEXT_ROUTER_FIELD: &str = "router";
/// Time-to-first-token target, in milliseconds (`nvext.router.ttft_target`).
pub const TTFT_TARGET_FIELD: &str = "ttft_target";
/// Inter-token-latency target, in milliseconds (`nvext.router.itl_target`).
pub const ITL_TARGET_FIELD: &str = "itl_target";
/// The scheduling hints inside `nvext` (`nvext.agent_hints`).
pub const NVEXT_AGENT_HINTS_FIELD: &str = "agent_hints";
/// The scheduling priority carrier the serving stack honours
/// (`nvext.agent_hints.priority`).
pub const PRIORITY_FIELD: &str = "priority";
/// The priority a request to a `dynamo` member carries when nothing set one:
/// realtime's. Batch deadlines sit below it, continuation resumes above.
pub const DEFAULT_PRIORITY: i32 = 0;
/// Key label naming the account (organisation) that owns the key. The account
/// policy in [`crate::target::Targets::accounts`] and an alias's overlays are
/// both keyed by its value.
pub const ACCOUNT_LABEL: &str = "account";
pub const ACCOUNT_NAME_LABEL: &str = "account_name";
/// Key label with the account's display name, for trace attributes. Optional;
/// [`ACCOUNT_NAME_LABEL`] is used when it is absent.
pub const ACCOUNT_DISPLAY_NAME_LABEL: &str = "account_display_name";

/// Where a request was routed, for analytics and tariff selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServingClass {
    /// The model's default destination.
    Standard,
}

impl ServingClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ServingClass::Standard => "standard",
        }
    }
}

impl fmt::Display for ServingClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Write [`DEFAULT_PRIORITY`] as `nvext.agent_hints.priority` unless the body
/// already carries a priority. One already present was set by someone
/// entitled to: the daemon (batch deadline, continuation resume), the
/// resolved hints, or the first hop when this crate runs again inside the serving
/// namespace; callers' own are scrubbed at dwctl's ingress. The rest of
/// `nvext` is kept.
pub fn stamp_default_priority(body: &mut serde_json::Map<String, serde_json::Value>) {
    let nvext = body
        .entry(NVEXT_FIELD)
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !nvext.is_object() {
        *nvext = serde_json::Value::Object(Default::default());
    }
    let hints = nvext
        .as_object_mut()
        .expect("nvext was just made an object")
        .entry(NVEXT_AGENT_HINTS_FIELD)
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !hints.is_object() {
        *hints = serde_json::Value::Object(Default::default());
    }
    hints
        .as_object_mut()
        .expect("agent_hints was just made an object")
        .entry(PRIORITY_FIELD)
        .or_insert_with(|| DEFAULT_PRIORITY.into());
}

/// Remove any router targets a caller put in `nvext`, returning whether
/// something was removed. Only the resolver sets them; anything inbound is an
/// attempt to steer the serving stack directly. The rest of `nvext.router`
/// (and of `nvext`) is left alone.
pub fn scrub_router_targets(nvext: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(router) = nvext
        .get_mut(NVEXT_ROUTER_FIELD)
        .and_then(|r| r.as_object_mut())
    else {
        return false;
    };
    let removed = [
        router.shift_remove(TTFT_TARGET_FIELD),
        router.shift_remove(ITL_TARGET_FIELD),
    ];
    if router.is_empty() {
        nvext.shift_remove(NVEXT_ROUTER_FIELD);
    }
    removed.iter().any(Option::is_some)
}

/// What kind of server a provider is. Decides who receives the hints and
/// who counts as external for a self-hosted-only organisation. Deliberately
/// its own fact rather than a reading of some other flag (`trusted`,
/// `accepts_scheduling_priority`): those correlate with it today and mean
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Self-hosted, behind the dynamo frontend. The only kind that receives
    /// the scheduling hints.
    Dynamo,
    /// Self-hosted, not behind dynamo.
    Hosted,
    /// A third-party provider. Never receives the hints; skipped for
    /// self-hosted-only organisations.
    #[default]
    External,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Dynamo => "dynamo",
            ProviderKind::Hosted => "hosted",
            ProviderKind::External => "external",
        }
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dynamo" => Ok(ProviderKind::Dynamo),
            "hosted" => Ok(ProviderKind::Hosted),
            "external" => Ok(ProviderKind::External),
            other => Err(format!("unknown provider kind '{other}'")),
        }
    }
}

/// Response extension: what the request was served as.
///
/// Present on every response that went through the resolver (which is every
/// response an upstream produced); absent on auth/validation rejections.
/// `requested` is reserved for explicitly requested classes and is currently
/// always `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingClassOutcome {
    pub requested: Option<ServingClass>,
    pub resolved: ServingClass,
}

/// An organisation's account settings for serving, synced once per org and
/// looked up by the key's [`ACCOUNT_LABEL`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountServing {
    /// Never fall over to an external provider.
    #[serde(default)]
    pub self_hosted_only: bool,
    /// Scheduling priority for the account's realtime requests on every
    /// alias, unless an overlay sets its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

impl AccountServing {
    /// True when nothing here changes a request; such accounts are not synced.
    pub fn is_empty(&self) -> bool {
        !self.self_hosted_only && self.priority.is_none()
    }
}

/// An account's per-alias overrides of its account settings. Lives on the
/// alias's pool spec, keyed by account id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingOverlay {
    /// Overrides the account's `self_hosted_only` on this alias.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
    /// Scheduling priority on this alias, overriding the account's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Time-to-first-token target on this alias, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u32>,
    /// Inter-token-latency target on this alias, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub itl_ms: Option<u32>,
}

/// Scheduling hints for one request. They change neither its class nor its
/// price; each field is written into the body only when set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServingHints {
    pub priority: Option<i32>,
    pub ttft_ms: Option<u32>,
    pub itl_ms: Option<u32>,
}

impl ServingHints {
    pub fn is_empty(&self) -> bool {
        self.priority.is_none() && self.ttft_ms.is_none() && self.itl_ms.is_none()
    }

    /// Write the set hints into a request body, overwriting existing values:
    /// `nvext.router.{ttft_target, itl_target}` and `nvext.agent_hints.priority`.
    pub fn stamp(&self, body: &mut serde_json::Map<String, serde_json::Value>) {
        if self.is_empty() {
            return;
        }
        let nvext = body
            .entry(NVEXT_FIELD)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if !nvext.is_object() {
            *nvext = serde_json::Value::Object(Default::default());
        }
        let nvext = nvext
            .as_object_mut()
            .expect("nvext was just made an object");
        if self.ttft_ms.is_some() || self.itl_ms.is_some() {
            let router = nvext
                .entry(NVEXT_ROUTER_FIELD)
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if !router.is_object() {
                *router = serde_json::Value::Object(Default::default());
            }
            let router = router
                .as_object_mut()
                .expect("router was just made an object");
            if let Some(ttft) = self.ttft_ms {
                router.insert(TTFT_TARGET_FIELD.to_string(), ttft.into());
            }
            if let Some(itl) = self.itl_ms {
                router.insert(ITL_TARGET_FIELD.to_string(), itl.into());
            }
        }
        if let Some(priority) = self.priority {
            let hints = nvext
                .entry(NVEXT_AGENT_HINTS_FIELD)
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if !hints.is_object() {
                *hints = serde_json::Value::Object(Default::default());
            }
            hints
                .as_object_mut()
                .expect("agent_hints was just made an object")
                .insert(PRIORITY_FIELD.to_string(), priority.into());
        }
    }
}

/// Key purposes whose requests are daemon legs: the ordering already travels
/// as a deadline-derived (batch, flex) or fixed (continuation resume)
/// priority in the body, and no hints are sent.
const DAEMON_PURPOSES: [&str; 2] = ["batch", "continuation"];

/// The outcome of resolving one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingResolution {
    pub resolved: ServingClass,
    /// Priority and latency targets sent to `dynamo` members.
    pub hints: ServingHints,
    /// Restrict the composite to its non-external members.
    pub self_hosted_only: bool,
}

impl ServingResolution {
    pub fn outcome(&self) -> ServingClassOutcome {
        ServingClassOutcome {
            requested: None,
            resolved: self.resolved,
        }
    }
}

/// Resolve the serving policy for one request.
///
/// * `account` — the owning organisation's settings, if it has any.
/// * `overlay` — the organisation's overrides on this alias, if any.
/// * `purpose` — the key's purpose label; daemon purposes get no hints.
pub fn resolve(
    account: Option<&AccountServing>,
    overlay: Option<&ServingOverlay>,
    purpose: Option<&str>,
) -> ServingResolution {
    let self_hosted_only = overlay
        .and_then(|o| o.self_hosted_only)
        .unwrap_or_else(|| account.is_some_and(|a| a.self_hosted_only));

    let daemon = purpose.is_some_and(|p| DAEMON_PURPOSES.contains(&p));
    ServingResolution {
        resolved: ServingClass::Standard,
        hints: if daemon {
            ServingHints::default()
        } else {
            ServingHints {
                priority: overlay
                    .and_then(|o| o.priority)
                    .or(account.and_then(|a| a.priority)),
                ttft_ms: overlay.and_then(|o| o.ttft_ms),
                itl_ms: overlay.and_then(|o| o.itl_ms),
            }
        },
        self_hosted_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deal() -> ServingOverlay {
        ServingOverlay {
            priority: Some(200),
            ttft_ms: Some(500),
            itl_ms: Some(20),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_set_resolves_to_standard_with_no_hints() {
        let r = resolve(None, None, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Standard);
        assert!(r.hints.is_empty());
        assert!(!r.self_hosted_only);
        assert_eq!(r.outcome().requested, None);
    }

    #[test]
    fn overlay_targets_are_hints_on_the_standard_class() {
        let o = deal();
        let r = resolve(None, Some(&o), Some("realtime"));
        assert_eq!(
            r.resolved,
            ServingClass::Standard,
            "targets never change the class"
        );
        assert_eq!(
            r.hints,
            ServingHints {
                priority: Some(200),
                ttft_ms: Some(500),
                itl_ms: Some(20)
            }
        );
        // a key without a purpose label is treated as realtime
        assert_eq!(resolve(None, Some(&o), None).hints, r.hints);
    }

    #[test]
    fn daemon_legs_never_receive_hints() {
        let o = deal();
        for purpose in ["batch", "continuation"] {
            let r = resolve(None, Some(&o), Some(purpose));
            assert_eq!(r.resolved, ServingClass::Standard, "{purpose}");
            assert!(r.hints.is_empty(), "{purpose}");
        }
    }

    #[test]
    fn self_hosted_only_comes_from_the_account_unless_the_overlay_overrides() {
        let a = AccountServing {
            self_hosted_only: true,
            ..Default::default()
        };
        assert!(resolve(Some(&a), None, Some("realtime")).self_hosted_only);
        // the daemon leg carries the restriction too: batch work must not spill either
        assert!(resolve(Some(&a), None, Some("batch")).self_hosted_only);
        let o = ServingOverlay {
            self_hosted_only: Some(false),
            ..Default::default()
        };
        assert!(!resolve(Some(&a), Some(&o), Some("realtime")).self_hosted_only);
    }

    #[test]
    fn hints_are_stamped_into_nvext_and_targets_scrubbed_from_it() {
        let mut body = serde_json::json!({
            "model": "m",
            "nvext": {"cache_control": {"enabled": true}, "router": {"ttft_target": 1}, "agent_hints": {"max_batch_size": 8}}
        });
        resolve(None, Some(&deal()), Some("realtime"))
            .hints
            .stamp(body.as_object_mut().unwrap());
        assert_eq!(body["nvext"]["router"]["ttft_target"], 500, "overwritten");
        assert_eq!(body["nvext"]["router"]["itl_target"], 20);
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 200);
        assert_eq!(
            body["nvext"]["agent_hints"]["max_batch_size"], 8,
            "the rest of the hints survive"
        );
        assert_eq!(body["nvext"]["cache_control"]["enabled"], true);

        let mut body = serde_json::json!({"model": "m"});
        ServingHints {
            ttft_ms: Some(1),
            itl_ms: Some(2),
            ..Default::default()
        }
        .stamp(body.as_object_mut().unwrap());
        assert_eq!(
            body["nvext"],
            serde_json::json!({"router": {"ttft_target": 1, "itl_target": 2}})
        );

        // scrubbing removes exactly the two targets, dropping an emptied router
        let nvext = body["nvext"].as_object_mut().unwrap();
        assert!(scrub_router_targets(nvext));
        assert!(nvext.is_empty());
        let mut nvext =
            serde_json::json!({"router": {"itl_target": 5, "other": 1}, "cache_control": {}});
        assert!(scrub_router_targets(nvext.as_object_mut().unwrap()));
        assert_eq!(
            nvext,
            serde_json::json!({"router": {"other": 1}, "cache_control": {}})
        );
        let mut nvext = serde_json::json!({"cache_control": {}});
        assert!(!scrub_router_targets(nvext.as_object_mut().unwrap()));
    }

    #[test]
    fn default_priority_is_stamped_only_where_none_is_present() {
        let mut body = serde_json::json!({"model": "m"});
        stamp_default_priority(body.as_object_mut().unwrap());
        assert_eq!(
            body["nvext"],
            serde_json::json!({"agent_hints": {"priority": 0}})
        );

        // a priority already present (batch deadline, resume, first hop) is kept
        let mut body = serde_json::json!({
            "nvext": {"cache_control": {"enabled": true}, "agent_hints": {"priority": -1234, "max_batch_size": 8}}
        });
        stamp_default_priority(body.as_object_mut().unwrap());
        assert_eq!(body["nvext"]["agent_hints"]["priority"], -1234);
        assert_eq!(body["nvext"]["agent_hints"]["max_batch_size"], 8);
        assert_eq!(body["nvext"]["cache_control"]["enabled"], true);

        // siblings survive when the priority is added
        let mut body = serde_json::json!({"nvext": {"agent_hints": {"max_batch_size": 8}}});
        stamp_default_priority(body.as_object_mut().unwrap());
        assert_eq!(
            body["nvext"]["agent_hints"],
            serde_json::json!({"max_batch_size": 8, "priority": 0})
        );

        // malformed carriers are replaced rather than panicking
        let mut body = serde_json::json!({"nvext": "x"});
        stamp_default_priority(body.as_object_mut().unwrap());
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 0);
        let mut body = serde_json::json!({"nvext": {"agent_hints": 7}});
        stamp_default_priority(body.as_object_mut().unwrap());
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 0);
    }

    #[test]
    fn account_and_overlay_priorities_resolve_as_standard_hints() {
        let account = AccountServing {
            priority: Some(100),
            ..Default::default()
        };
        let r = resolve(Some(&account), None, Some("realtime"));
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(
            r.hints.priority,
            Some(100),
            "the account default applies everywhere"
        );

        let overlay = ServingOverlay {
            priority: Some(300),
            ttft_ms: Some(900),
            itl_ms: Some(25),
            ..Default::default()
        };
        let r = resolve(Some(&account), Some(&overlay), Some("realtime"));
        assert_eq!(
            r.resolved,
            ServingClass::Standard,
            "hints never change the class"
        );
        assert_eq!(
            r.hints,
            ServingHints {
                priority: Some(300),
                ttft_ms: Some(900),
                itl_ms: Some(25)
            }
        );

        // Daemon legs keep their deadline priority.
        assert!(
            resolve(Some(&account), Some(&overlay), Some("batch"))
                .hints
                .is_empty()
        );
    }

    #[test]
    fn hints_stamp_only_what_is_set() {
        let mut body =
            serde_json::json!({"model": "m", "nvext": {"agent_hints": {"max_batch_size": 8}}});
        ServingHints {
            priority: Some(500),
            ..Default::default()
        }
        .stamp(body.as_object_mut().unwrap());
        assert_eq!(
            body["nvext"]["agent_hints"],
            serde_json::json!({"max_batch_size": 8, "priority": 500})
        );
        assert!(body["nvext"].get("router").is_none());

        let mut body = serde_json::json!({"model": "m"});
        ServingHints {
            ttft_ms: Some(1000),
            ..Default::default()
        }
        .stamp(body.as_object_mut().unwrap());
        assert_eq!(
            body["nvext"],
            serde_json::json!({"router": {"ttft_target": 1000}})
        );

        let mut body = serde_json::json!({"model": "m"});
        ServingHints::default().stamp(body.as_object_mut().unwrap());
        assert_eq!(
            body,
            serde_json::json!({"model": "m"}),
            "nothing set, nothing written"
        );
    }

    #[test]
    fn kinds_and_outcomes() {
        assert!(AccountServing::default().is_empty());
        assert_eq!(ProviderKind::default(), ProviderKind::External);
        assert_eq!("dynamo".parse::<ProviderKind>(), Ok(ProviderKind::Dynamo));
        assert!("cloud".parse::<ProviderKind>().is_err());
        assert_eq!(ServingClass::Standard.as_str(), "standard");
        assert_eq!(
            serde_json::to_string(&ServingClass::Standard).unwrap(),
            "\"standard\""
        );
    }

    #[test]
    fn retired_class_fields_in_stored_config_are_ignored() {
        // Configuration written before the class presets were retired must
        // still load: the old fields are dropped, the rest is kept.
        let a: AccountServing = serde_json::from_str(
            r#"{"granted": ["interactive"], "default_class": "throughput", "self_hosted_only": true}"#,
        )
        .unwrap();
        assert!(a.self_hosted_only);
        let o: ServingOverlay = serde_json::from_str(
            r#"{"default_class": "interactive", "targets": {"ttft_ms": 800, "itl_ms": 30}, "priority": 5}"#,
        )
        .unwrap();
        assert_eq!(o.priority, Some(5));
        assert_eq!(
            serde_json::to_string(&ProviderKind::Dynamo).unwrap(),
            "\"dynamo\""
        );
    }
}

/// End-to-end behaviour through the request handler: what reaches an
/// upstream member for each resolution outcome.
#[cfg(test)]
mod handler_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;

    use super::*;
    use crate::handlers::target_message_handler;
    use crate::load_balancer::{Provider, ProviderPool};
    use crate::strict::build_strict_router;
    use crate::target::{FallbackConfig, LoadBalanceStrategy, Target, TargetPools, Targets};
    use crate::test_utils::{MockHttpClient, MockRequest};
    use crate::{AppState, build_router};

    const KEY: &str = "sk-serving-test";
    const ACCOUNT: &str = "org-1";
    const ALIAS: &str = "gpt-4";
    const OK_BODY: &str =
        r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4","choices":[]}"#;

    /// A member of the composite: `(url, kind)`.
    type Member = (&'static str, ProviderKind);

    struct Setup {
        members: Vec<Member>,
        purpose: &'static str,
        account: Option<AccountServing>,
        overlay: Option<ServingOverlay>,
    }

    fn setup(members: &[Member]) -> Setup {
        Setup {
            members: members.to_vec(),
            purpose: "realtime",
            account: None,
            overlay: None,
        }
    }

    fn with_deal(members: &[Member]) -> Setup {
        let mut s = setup(members);
        s.overlay = Some(ServingOverlay {
            priority: Some(200),
            ttft_ms: Some(500),
            itl_ms: Some(20),
            ..Default::default()
        });
        s
    }

    fn targets(s: &Setup) -> Targets {
        let providers = s
            .members
            .iter()
            .map(|(url, kind)| {
                let t = Target::builder()
                    .url(url.parse().unwrap())
                    .kind(*kind)
                    .build();
                Provider::new(t, 1)
            })
            .collect();
        let mut overlays = HashMap::new();
        if let Some(o) = &s.overlay {
            overlays.insert(ACCOUNT.to_string(), o.clone());
        }
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
        .with_overlays(overlays);
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            ALIAS.to_string(),
            TargetPools::with_pools(pool, HashMap::new()),
        );
        let key_labels = Arc::new(DashMap::new());
        key_labels.insert(
            KEY.to_string(),
            HashMap::from([
                ("purpose".to_string(), s.purpose.to_string()),
                (ACCOUNT_LABEL.to_string(), ACCOUNT.to_string()),
            ]),
        );
        let accounts = Arc::new(DashMap::new());
        if let Some(a) = &s.account {
            accounts.insert(ACCOUNT.to_string(), a.clone());
        }
        Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels,
            accounts,
            strict_mode: false,
            http_pool_config: None,
        }
    }

    fn body(req: &MockRequest) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap()
    }

    /// The router targets and priority that reached the upstream, if any.
    fn sent(req: &MockRequest) -> Option<(u64, u64, Option<i64>)> {
        let body = body(req);
        let router = body.get("nvext")?.get("router")?;
        Some((
            router["ttft_target"].as_u64()?,
            router["itl_target"].as_u64()?,
            body["nvext"]
                .get("agent_hints")
                .and_then(|h| h.get("priority"))
                .and_then(|p| p.as_i64()),
        ))
    }

    async fn post(
        server: &TestServer,
        model: &str,
        body_extra: serde_json::Value,
    ) -> axum_test::TestResponse {
        let mut payload = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
        if let Some(extra) = body_extra.as_object() {
            for (k, v) in extra {
                payload[k] = v.clone();
            }
        }
        server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {KEY}"))
            .json(&payload)
            .await
    }

    const DYNAMO: Member = ("https://dynamo.example.com/", ProviderKind::Dynamo);
    const EXTERNAL: Member = ("https://third-party.example.com/", ProviderKind::External);
    const HOSTED: Member = ("https://hosted.example.com/", ProviderKind::Hosted);

    fn server(s: &Setup, mock: &MockHttpClient) -> TestServer {
        TestServer::new(build_router(AppState::with_client(
            targets(s),
            mock.clone(),
        )))
        .unwrap()
    }

    #[tokio::test]
    async fn strict_handlers_send_overlay_hints() {
        for (path, payload) in [
            ("/chat/completions", json!({"model":ALIAS,"messages":[]})),
            ("/completions", json!({"model":ALIAS,"prompt":"hi"})),
            ("/embeddings", json!({"model":ALIAS,"input":"hi"})),
            ("/responses", json!({"model":ALIAS,"input":"hi"})),
        ] {
            let s = with_deal(&[DYNAMO]);
            // An upstream failure avoids coupling this forwarding test to
            // four different success-response schemas.
            let mock =
                MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"failed"}"#);
            let router = build_strict_router(AppState::with_client(targets(&s), mock.clone()));
            let srv = TestServer::new(router).unwrap();
            let response = srv
                .post(path)
                .add_header("authorization", format!("Bearer {KEY}"))
                .json(&payload)
                .await;
            assert_eq!(response.status_code(), 502, "{path}");
            assert_eq!(
                sent(&mock.get_requests()[0]),
                Some((500, 20, Some(200))),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn no_eligible_provider_retains_the_resolved_class() {
        let mut s = with_deal(&[EXTERNAL]);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("authorization", format!("Bearer {KEY}"))
            .body(Body::from(json!({"model":ALIAS,"messages":[]}).to_string()))
            .unwrap();
        let error = target_message_handler(
            State(AppState::with_client(targets(&s), mock.clone())),
            request,
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.serving_outcome.unwrap().resolved,
            ServingClass::Standard
        );
        assert!(mock.get_requests().is_empty());
    }

    #[tokio::test]
    async fn only_dynamo_members_receive_the_hints() {
        for member in [EXTERNAL, HOSTED] {
            let s = with_deal(&[member]);
            let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
            let srv = server(&s, &mock);
            assert_eq!(post(&srv, ALIAS, json!({})).await.status_code(), 200);
            let req = &mock.get_requests()[0];
            assert_eq!(sent(req), None, "{:?}", member.1);
            assert!(body(req).get("nvext").is_none(), "{:?}", member.1);
            assert_eq!(body(req)["model"], "gpt-4");
        }
    }

    #[tokio::test]
    async fn account_and_overlay_hints_reach_dynamo_members_as_standard() {
        let mut s = setup(&[DYNAMO]);
        s.account = Some(AccountServing {
            priority: Some(100),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        assert_eq!(
            post(&server(&s, &mock), ALIAS, json!({}))
                .await
                .status_code(),
            200
        );
        assert_eq!(
            body(&mock.get_requests()[0])["nvext"],
            json!({"agent_hints": {"priority": 100}})
        );

        s.overlay = Some(ServingOverlay {
            priority: Some(300),
            ttft_ms: Some(900),
            itl_ms: Some(25),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        assert_eq!(
            post(&server(&s, &mock), ALIAS, json!({}))
                .await
                .status_code(),
            200
        );
        assert_eq!(sent(&mock.get_requests()[0]), Some((900, 25, Some(300))));

        // An external member never receives them.
        let mut s = setup(&[EXTERNAL]);
        s.account = Some(AccountServing {
            priority: Some(100),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        assert_eq!(
            post(&server(&s, &mock), ALIAS, json!({}))
                .await
                .status_code(),
            200
        );
        assert!(body(&mock.get_requests()[0]).get("nvext").is_none());
    }

    #[tokio::test]
    async fn no_policy_at_all_sends_only_realtimes_priority() {
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, ALIAS, json!({})).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(body(req)["nvext"], json!({"agent_hints": {"priority": 0}}));
        assert_eq!(body(req)["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn a_non_json_body_reaches_a_dynamo_member_untouched() {
        // Multipart uploads carry no hints: they are forwarded as they came,
        // never refused.
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let raw = "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-4\r\n--b--\r\n";
        let response = srv
            .post("/v1/audio/transcriptions")
            .add_header("authorization", format!("Bearer {KEY}"))
            .add_header("model-override", ALIAS)
            .add_header("content-type", "multipart/form-data; boundary=b")
            .bytes(raw.as_bytes().to_vec().into())
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert_eq!(mock.get_requests()[0].body, raw.as_bytes());
    }

    #[tokio::test]
    async fn a_colon_suffix_is_not_a_class_selector() {
        // Retired class suffixes are ordinary (unknown) model names now.
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:interactive", json!({})).await;
        assert_eq!(response.status_code(), 404, "{}", response.text());
        assert!(mock.get_requests().is_empty());
    }

    #[tokio::test]
    async fn inbound_targets_pass_through_and_a_resolution_overwrites_them() {
        // No policy: this crate is also the hop inside the serving namespace and
        // must forward what the first hop stamped; clients are scrubbed at dwctl.
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let inbound = json!({"nvext": {"router": {"ttft_target": 9, "itl_target": 9}, "agent_hints": {"priority": 7}}});
        let response = post(&srv, ALIAS, inbound.clone()).await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(
            sent(&mock.get_requests()[0]),
            Some((9, 9, Some(7))),
            "second-hop pass-through"
        );

        let s = with_deal(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, ALIAS, inbound).await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(
            sent(&mock.get_requests()[0]),
            Some((500, 20, Some(200))),
            "the hints are overwritten"
        );
    }

    #[tokio::test]
    async fn daemon_purpose_keys_get_no_hints() {
        let mut s = with_deal(&[DYNAMO]);
        s.purpose = "batch";
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        // the deadline priority the daemon injected travels untouched
        let response = post(
            &srv,
            ALIAS,
            json!({"nvext": {"agent_hints": {"priority": -1234}}}),
        )
        .await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(sent(req), None);
        assert_eq!(body(req)["nvext"]["agent_hints"]["priority"], -1234);
    }

    #[tokio::test]
    async fn an_upstream_failure_still_reports_the_resolved_class() {
        let s = with_deal(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        // Straight through the router: the extension lives on the axum
        // response, which the test-server wrapper does not expose.
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {KEY}"))
            .body(axum::body::Body::from(
                json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]})
                    .to_string(),
            ))
            .unwrap();
        let response = build_router(AppState::with_client(targets(&s), mock.clone()))
            .oneshot(request)
            .await
            .unwrap();
        assert!(response.status().is_server_error());
        let outcome = response
            .extensions()
            .get::<ServingClassOutcome>()
            .copied()
            .expect("the outcome rides on error responses too, for analytics");
        assert_eq!(outcome.requested, None);
        assert_eq!(outcome.resolved, ServingClass::Standard);
    }

    #[tokio::test]
    async fn self_hosted_only_accounts_never_reach_an_external_member() {
        let members = [DYNAMO, EXTERNAL];
        // Control: the self-hosted member fails and the pool falls over.
        let s = setup(&members);
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", json!({})).await;
        assert_eq!(
            mock.get_requests().len(),
            2,
            "without the setting both members are tried"
        );

        let mut s = setup(&members);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4", json!({})).await;
        assert!(
            response.status_code().is_server_error(),
            "{}",
            response.status_code()
        );
        let requests = mock.get_requests();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|r| r.uri.starts_with("https://dynamo.example.com/")),
            "the external member is never attempted; the attempt budget is spent on eligible members"
        );

        // An alias with no eligible member at all is refused as such, not as
        // a pool at capacity.
        let mut s = setup(&[EXTERNAL]);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4", json!({})).await;
        assert_eq!(response.status_code(), 503);
        let err: serde_json::Value = response.json();
        assert_eq!(err["error"]["code"], "no_eligible_provider", "{err}");
        assert!(mock.get_requests().is_empty());

        // A hosted (non-dynamo, self-hosted) member is still eligible.
        let mut s = setup(&[DYNAMO, HOSTED]);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", json!({})).await;
        assert_eq!(mock.get_requests().len(), 2);

        // A per-alias overlay can lift the account-wide restriction.
        let mut s = setup(&members);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        s.overlay = Some(ServingOverlay {
            self_hosted_only: Some(false),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", json!({})).await;
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn a_named_pool_request_is_resolved_against_the_alias_policy() {
        // Overlays are declared on the alias's default pool; a request served
        // by a named pool (here: completions) must still see them.
        let s = with_deal(&[DYNAMO]);
        let mock = MockHttpClient::new(
            StatusCode::OK,
            r#"{"id":"cmpl-1","object":"text_completion","model":"gpt-4","choices":[]}"#,
        );
        let targets = targets(&s);
        let completions = ProviderPool::with_config(
            vec![Provider::new(
                Target::builder()
                    .url("https://dynamo-completions.example.com/".parse().unwrap())
                    .kind(ProviderKind::Dynamo)
                    .accepts_scheduling_priority(true)
                    .build(),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let default_pool = targets.targets.get(ALIAS).unwrap().default_pool().clone();
        targets.targets.insert(
            ALIAS.to_string(),
            TargetPools::with_pools(
                default_pool,
                HashMap::from([("completions".to_string(), completions)]),
            ),
        );
        let srv =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();
        let response = srv
            .post("/v1/completions")
            .add_header("authorization", format!("Bearer {KEY}"))
            .json(&json!({"model": "gpt-4", "prompt": "hi"}))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let req = &mock.get_requests()[0];
        assert!(
            req.uri
                .starts_with("https://dynamo-completions.example.com/"),
            "{}",
            req.uri
        );
        assert_eq!(sent(req), Some((500, 20, Some(200))));
    }

    #[tokio::test]
    async fn a_named_pool_member_that_refuses_scheduling_fields_gets_no_default_priority() {
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(
            StatusCode::OK,
            r#"{"id":"cmpl-1","object":"text_completion","model":"gpt-4","choices":[]}"#,
        );
        let targets = targets(&s);
        let completions = ProviderPool::with_config(
            vec![Provider::new(
                Target::builder()
                    .url("https://dynamo-completions.example.com/".parse().unwrap())
                    .kind(ProviderKind::Dynamo)
                    .accepts_scheduling_priority(false)
                    .build(),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let default_pool = targets.targets.get(ALIAS).unwrap().default_pool().clone();
        targets.targets.insert(
            ALIAS.to_string(),
            TargetPools::with_pools(
                default_pool,
                HashMap::from([("completions".to_string(), completions)]),
            ),
        );
        let srv =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();
        let response = srv
            .post("/v1/completions")
            .add_header("authorization", format!("Bearer {KEY}"))
            .json(&json!({"model": "gpt-4", "prompt": "hi"}))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(body(&mock.get_requests()[0]).get("nvext").is_none());
    }

    #[test]
    fn accounts_and_overlays_round_trip_through_config_json() {
        // Retired fields (`serving_classes`, `granted`, `default_class`,
        // overlay `targets`) in a stored config are ignored rather than rejected.
        let json = json!({
            "targets": {
                "gpt-4": {
                    "providers": [{"url": "https://dynamo.example.com/", "kind": "dynamo"}],
                    "serving_classes": {"interactive": {"ttft_ms": 500, "itl_ms": 20, "priority": 200}},
                    "overlays": {
                        "org-1": {"default_class": "interactive", "self_hosted_only": false},
                        "org-2": {"targets": {"ttft_ms": 800, "itl_ms": 30}, "priority": 300, "ttft_ms": 800, "itl_ms": 30}
                    }
                }
            },
            "accounts": {"org-1": {"granted": ["interactive", "throughput"], "self_hosted_only": true}},
            "auth": {"global_keys": [], "key_definitions": {"k": {"key": "sk-1", "labels": {"account": "org-1"}}}}
        });
        let config: crate::target::ConfigFile = serde_json::from_value(json).unwrap();
        let targets = Targets::from_config(config).unwrap();
        let account = targets.accounts.get("org-1").unwrap();
        assert!(account.self_hosted_only);
        let pools = targets.targets.get("gpt-4").unwrap();
        let overlays = pools.default_pool().overlays();
        assert_eq!(overlays["org-1"].self_hosted_only, Some(false));
        assert_eq!(
            (
                overlays["org-2"].priority,
                overlays["org-2"].ttft_ms,
                overlays["org-2"].itl_ms
            ),
            (Some(300), Some(800), Some(30))
        );
        assert_eq!(
            pools.default_pool().providers()[0].target.kind,
            ProviderKind::Dynamo
        );
        // A plain config still parses: no accounts, no overlays, kind defaults to external.
        let config: crate::target::ConfigFile =
            serde_json::from_value(json!({"targets": {"m": {"url": "https://x.example.com/"}}}))
                .unwrap();
        let targets = Targets::from_config(config).unwrap();
        assert!(targets.accounts.is_empty());
        assert_eq!(
            targets.targets.get("m").unwrap().default_pool().providers()[0]
                .target
                .kind,
            ProviderKind::External
        );
    }
}
