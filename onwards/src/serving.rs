//! Serving classes: the per-request dispatch mode and how it is resolved.
//!
//! A serving class names how a request wants to be served. `interactive`
//! (per-stream speed, fast first token) and `throughput` (aggregate volume)
//! are *elevated* classes; `standard` is what everyone gets when nothing is
//! set. In v1 the class maps directly onto the serving stack's two
//! interactivity pools and a priority band: an elevated class is stamped on
//! the dynamo member as a pool tag plus priority, `standard` carries nothing
//! and is byte-identical to today.
//!
//! Two facts gate an elevated class, both cheap to administer:
//!
//! * the **organisation holds it** — an account setting synced once per org
//!   ([`AccountServing::granted`]);
//! * the **model offers it** — declared on the alias, flipped when its pools
//!   exist ([`crate::target::PoolSpec::serving_classes`]).
//!
//! Resolution is one flat field:
//!
//! ```text
//! requested = model suffix, else the org's overlay default for this alias,
//!             else the org's account default, else none
//! resolved  = requested if the org holds it AND the alias offers it, else standard
//! ```
//!
//! **Strict mode.** A class named on the request itself is a promise, never a
//! hint: if the org does not hold it, or the alias does not offer it, the
//! request is rejected rather than quietly served as `standard`. Only the
//! silent inputs (an account default, an overlay default) degrade silently,
//! because nobody asked for anything on that request.
//!
//! Daemon legs (batch and flex on the batch-purpose key, continuation resume
//! legs) always resolve to `standard`: their existing deadline priorities
//! carry the ordering and no tag is sent.
//!
//! Everything a client could send to steer this itself — the pool tag, the
//! priority header, the `nvext` pool field — is stripped at dwctl's ingress;
//! only the resolver sets them.

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
/// Key label naming the account (organisation) that owns the key. The account
/// policy in [`crate::target::Targets::accounts`] and an alias's overlays are
/// both keyed by its value.
pub const ACCOUNT_LABEL: &str = "account";

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

    /// Elevated classes are the ones a model offers and an org holds.
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
    /// is 0, today's implicit realtime value. Interactive sits above
    /// throughput because borrowing can land both in one pool.
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
/// an unknown class is an error, never silently ignored.
pub fn split_class_suffix(model: &str) -> Result<(&str, Option<ServingClass>), UnknownServingClass> {
    match model.rsplit_once(SUFFIX_SEPARATOR) {
        None => Ok((model, None)),
        Some((alias, suffix)) => suffix.parse().map(|class| (alias, Some(class))),
    }
}

/// What kind of server a provider is. Decides who receives the envelope and
/// who counts as external for a self-hosted-only organisation. Deliberately
/// its own fact rather than a reading of some other flag (`trusted`,
/// `accepts_scheduling_priority`): those correlate with it today and mean
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Self-hosted, behind the dynamo frontend. The only kind that receives
    /// the serving-class envelope.
    Dynamo,
    /// Self-hosted, not behind dynamo.
    Hosted,
    /// A third-party provider. Never receives the envelope; skipped for
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

/// An organisation's account settings for serving, synced once per org and
/// looked up by the key's [`ACCOUNT_LABEL`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountServing {
    /// Elevated classes the organisation holds.
    #[serde(default)]
    pub granted: Vec<ServingClass>,
    /// Class its requests ask for when the request names none.
    #[serde(default)]
    pub default_class: Option<ServingClass>,
    /// Never fall over to an external provider.
    #[serde(default)]
    pub self_hosted_only: bool,
}

impl AccountServing {
    /// True when nothing here changes a request; such accounts are not synced.
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty() && self.default_class.is_none() && !self.self_hosted_only
    }
}

/// An organisation's per-alias overrides of its account settings. Lives on
/// the alias's pool spec, keyed by account id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingOverlay {
    /// Overrides the account's default class on this alias.
    #[serde(default)]
    pub default_class: Option<ServingClass>,
    /// Overrides the account's `self_hosted_only` on this alias.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
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
    /// Restrict the composite to its non-external members.
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

/// Why an explicitly requested class was refused (strict mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassRejection {
    /// The organisation does not hold the class.
    NotHeld { class: ServingClass },
    /// The organisation holds the class but this alias does not offer it.
    NotOffered { class: ServingClass, alias: String },
}

impl fmt::Display for ClassRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassRejection::NotHeld { class } => {
                write!(f, "Serving class '{class}' is not available for this account.")
            }
            ClassRejection::NotOffered { class, alias } => {
                write!(f, "Serving class '{class}' is not available on model '{alias}'.")
            }
        }
    }
}

impl std::error::Error for ClassRejection {}

/// Resolve the serving class for one request.
///
/// * `suffix` — the class named on the request itself, if any.
/// * `account` — the owning organisation's settings, if it has any.
/// * `overlay` — the organisation's overrides on this alias, if any.
/// * `alias` — the model alias the customer called (for messages).
/// * `offered` — the elevated classes the alias offers.
/// * `purpose` — the key's purpose label; daemon purposes resolve to standard.
pub fn resolve(
    suffix: Option<ServingClass>,
    account: Option<&AccountServing>,
    overlay: Option<&ServingOverlay>,
    alias: &str,
    offered: &[ServingClass],
    purpose: Option<&str>,
) -> Result<ServingResolution, ClassRejection> {
    let self_hosted_only = overlay
        .and_then(|o| o.self_hosted_only)
        .unwrap_or_else(|| account.is_some_and(|a| a.self_hosted_only));

    if purpose.is_some_and(|p| DAEMON_PURPOSES.contains(&p)) {
        return Ok(ServingResolution {
            requested: None,
            resolved: ServingClass::Standard,
            self_hosted_only,
        });
    }

    let held = |class: ServingClass| account.is_some_and(|a| a.granted.contains(&class));

    // Strict mode: a class named on the request is honoured or refused.
    if let Some(class) = suffix
        && class.is_elevated()
    {
        if !held(class) {
            return Err(ClassRejection::NotHeld { class });
        }
        if !offered.contains(&class) {
            return Err(ClassRejection::NotOffered {
                class,
                alias: alias.to_string(),
            });
        }
    }

    let requested = suffix
        .or(overlay.and_then(|o| o.default_class))
        .or(account.and_then(|a| a.default_class));

    let resolved = match requested {
        Some(class) if class.is_elevated() && held(class) && offered.contains(&class) => class,
        _ => ServingClass::Standard,
    };

    Ok(ServingResolution {
        requested,
        resolved,
        self_hosted_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [ServingClass; 2] = [ServingClass::Interactive, ServingClass::Throughput];

    fn account(granted: &[ServingClass], default: Option<ServingClass>) -> AccountServing {
        AccountServing {
            granted: granted.to_vec(),
            default_class: default,
            self_hosted_only: false,
        }
    }

    fn realtime(
        suffix: Option<ServingClass>,
        account: Option<&AccountServing>,
        overlay: Option<&ServingOverlay>,
        offered: &[ServingClass],
    ) -> Result<ServingResolution, ClassRejection> {
        resolve(suffix, account, overlay, "m", offered, Some("realtime"))
    }

    #[test]
    fn suffix_is_split_and_unknown_class_is_an_error() {
        assert_eq!(split_class_suffix("zai-org/GLM-5.2"), Ok(("zai-org/GLM-5.2", None)));
        assert_eq!(
            split_class_suffix("zai-org/GLM-5.2:interactive"),
            Ok(("zai-org/GLM-5.2", Some(ServingClass::Interactive)))
        );
        assert_eq!(split_class_suffix("m:standard"), Ok(("m", Some(ServingClass::Standard))));
        let err = split_class_suffix("m:fast").unwrap_err();
        assert_eq!(err, UnknownServingClass("fast".to_string()));
        assert!(err.to_string().contains("interactive, throughput, standard"));
    }

    #[test]
    fn nothing_set_resolves_to_standard_with_no_request() {
        let r = realtime(None, None, None, &BOTH).unwrap();
        assert_eq!(r.requested, None);
        assert_eq!(r.resolved, ServingClass::Standard);
        assert!(!r.self_hosted_only);
    }

    #[test]
    fn precedence_is_suffix_then_overlay_then_account() {
        let a = account(&BOTH, Some(ServingClass::Interactive));
        let o = ServingOverlay {
            default_class: Some(ServingClass::Throughput),
            self_hosted_only: None,
        };
        // suffix wins over everything
        let r = realtime(Some(ServingClass::Interactive), Some(&a), Some(&o), &BOTH).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
        // overlay default wins over the account default
        let r = realtime(None, Some(&a), Some(&o), &BOTH).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Throughput));
        assert_eq!(r.resolved, ServingClass::Throughput);
        // account default alone
        let r = realtime(None, Some(&a), None, &BOTH).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn explicit_standard_suffix_opts_down_and_is_recorded() {
        let a = account(&BOTH, Some(ServingClass::Interactive));
        let r = realtime(Some(ServingClass::Standard), Some(&a), None, &BOTH).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Standard));
        assert_eq!(r.resolved, ServingClass::Standard);
    }

    #[test]
    fn strict_mode_refuses_an_explicit_class_the_org_does_not_hold_or_the_model_does_not_offer() {
        // not held (no account at all, or account without it)
        let err = realtime(Some(ServingClass::Interactive), None, None, &BOTH).unwrap_err();
        assert_eq!(
            err,
            ClassRejection::NotHeld {
                class: ServingClass::Interactive
            }
        );
        let a = account(&[ServingClass::Throughput], None);
        let err = realtime(Some(ServingClass::Interactive), Some(&a), None, &BOTH).unwrap_err();
        assert!(matches!(err, ClassRejection::NotHeld { .. }));
        assert!(err.to_string().contains("not available for this account"));
        // held but the model does not offer it
        let a = account(&BOTH, None);
        let err = realtime(Some(ServingClass::Interactive), Some(&a), None, &[]).unwrap_err();
        assert_eq!(
            err,
            ClassRejection::NotOffered {
                class: ServingClass::Interactive,
                alias: "m".to_string()
            }
        );
        assert!(err.to_string().contains("not available on model 'm'"));
    }

    #[test]
    fn silent_defaults_degrade_to_standard_but_stay_visible() {
        // account default on a model that offers nothing: served standard, requested recorded
        let a = account(&BOTH, Some(ServingClass::Interactive));
        let r = realtime(None, Some(&a), None, &[]).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Interactive));
        assert_eq!(r.resolved, ServingClass::Standard);
        // overlay default for a class the org does not hold: same
        let a = account(&[], None);
        let o = ServingOverlay {
            default_class: Some(ServingClass::Throughput),
            self_hosted_only: None,
        };
        let r = realtime(None, Some(&a), Some(&o), &BOTH).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Throughput));
        assert_eq!(r.resolved, ServingClass::Standard);
    }

    #[test]
    fn daemon_legs_always_resolve_to_standard_even_with_a_suffix() {
        let a = account(&BOTH, Some(ServingClass::Interactive));
        for purpose in DAEMON_PURPOSES {
            let r = resolve(Some(ServingClass::Interactive), Some(&a), None, "m", &BOTH, Some(purpose)).unwrap();
            assert_eq!(r.requested, None, "{purpose}");
            assert_eq!(r.resolved, ServingClass::Standard, "{purpose}");
        }
        // playground behaves like realtime
        let r = resolve(None, Some(&a), None, "m", &BOTH, Some("playground")).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn self_hosted_only_comes_from_the_account_unless_the_overlay_overrides() {
        let mut a = account(&[], None);
        a.self_hosted_only = true;
        assert!(realtime(None, Some(&a), None, &[]).unwrap().self_hosted_only);
        // the daemon leg carries the restriction too: batch work must not spill either
        assert!(
            resolve(None, Some(&a), None, "m", &[], Some("batch"))
                .unwrap()
                .self_hosted_only
        );
        let o = ServingOverlay {
            default_class: None,
            self_hosted_only: Some(false),
        };
        assert!(!realtime(None, Some(&a), Some(&o), &[]).unwrap().self_hosted_only);
    }

    #[test]
    fn bands_tags_and_kinds() {
        assert_eq!(ServingClass::Interactive.pool_tag(), Some("interactive"));
        assert_eq!(ServingClass::Throughput.pool_tag(), Some("throughput"));
        assert_eq!(ServingClass::Standard.pool_tag(), None);
        assert_eq!(ServingClass::Interactive.priority_band(), 200);
        assert_eq!(ServingClass::Throughput.priority_band(), 100);
        assert_eq!(ServingClass::Standard.priority_band(), 0);
        assert!(AccountServing::default().is_empty());
        assert_eq!(ProviderKind::default(), ProviderKind::External);
        assert_eq!("dynamo".parse::<ProviderKind>(), Ok(ProviderKind::Dynamo));
        assert!("cloud".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn serde_uses_lowercase_names() {
        let json = serde_json::to_string(&AccountServing {
            granted: vec![ServingClass::Interactive],
            ..Default::default()
        })
        .unwrap();
        assert!(json.contains("\"granted\":[\"interactive\"]"));
        let back: AccountServing = serde_json::from_str(&json).unwrap();
        assert_eq!(back.granted, vec![ServingClass::Interactive]);
        assert_eq!(serde_json::to_string(&ProviderKind::Dynamo).unwrap(), "\"dynamo\"");
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
    const ACCOUNT: &str = "org-1";
    const ALIAS: &str = "gpt-4";
    const OK_BODY: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4","choices":[]}"#;

    /// A member of the composite: `(url, kind)`.
    type Member = (&'static str, ProviderKind);

    struct Setup {
        members: Vec<Member>,
        offered: Vec<ServingClass>,
        purpose: &'static str,
        account: Option<AccountServing>,
        overlay: Option<ServingOverlay>,
    }

    fn setup(members: &[Member]) -> Setup {
        Setup {
            members: members.to_vec(),
            offered: Vec::new(),
            purpose: "realtime",
            account: None,
            overlay: None,
        }
    }

    fn targets(s: &Setup) -> Targets {
        let providers = s
            .members
            .iter()
            .map(|(url, kind)| {
                let t = Target::builder().url(url.parse().unwrap()).kind(*kind).build();
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
        .with_serving(s.offered.clone(), overlays);
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(ALIAS.to_string(), TargetPools::with_pools(pool, HashMap::new()));
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

    fn holds(classes: &[ServingClass]) -> AccountServing {
        AccountServing {
            granted: classes.to_vec(),
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
    const DYNAMO: Member = ("https://dynamo.example.com/", ProviderKind::Dynamo);
    const EXTERNAL: Member = ("https://third-party.example.com/", ProviderKind::External);
    const HOSTED: Member = ("https://hosted.example.com/", ProviderKind::Hosted);

    fn server(s: &Setup, mock: &MockHttpClient) -> TestServer {
        TestServer::new(build_router(AppState::with_client(targets(s), mock.clone()))).unwrap()
    }

    #[tokio::test]
    async fn elevated_class_is_stamped_on_a_dynamo_member_and_the_suffix_is_stripped() {
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        s.account = Some(holds(&BOTH));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);

        let response = post(&srv, "gpt-4:interactive", &[]).await;
        assert_eq!(response.status_code(), 200);
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("interactive"));
        assert_eq!(header(req, PRIORITY_HEADER), Some("200"));
        assert_eq!(body(req)["model"], "gpt-4", "the suffix never reaches an upstream");
        assert_eq!(body(req)["messages"][0]["content"], "hi", "the rest of the body is untouched");

        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4:throughput", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("throughput"));
        assert_eq!(header(req, PRIORITY_HEADER), Some("100"));
    }

    #[tokio::test]
    async fn only_dynamo_members_receive_the_envelope() {
        for member in [EXTERNAL, HOSTED] {
            let mut s = setup(&[member]);
            s.offered = BOTH.to_vec();
            s.account = Some(holds(&BOTH));
            let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
            let srv = server(&s, &mock);
            assert_eq!(post(&srv, "gpt-4:interactive", &[]).await.status_code(), 200);
            let req = &mock.get_requests()[0];
            assert_eq!(header(req, POOL_TAG_HEADER), None, "{:?}", member.1);
            assert_eq!(header(req, PRIORITY_HEADER), None, "{:?}", member.1);
            assert_eq!(body(req)["model"], "gpt-4");
        }
    }

    #[tokio::test]
    async fn account_default_applies_silently_and_degrades_silently() {
        // held and offered: the default elevates without a suffix
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        s.account = Some(AccountServing {
            granted: BOTH.to_vec(),
            default_class: Some(ServingClass::Throughput),
            self_hosted_only: false,
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", &[]).await.status_code(), 200);
        assert_eq!(header(&mock.get_requests()[0], POOL_TAG_HEADER), Some("throughput"));

        // same account, a model that offers nothing: served, untagged
        s.offered = Vec::new();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", &[]).await.status_code(), 200);
        assert_eq!(header(&mock.get_requests()[0], POOL_TAG_HEADER), None);

        // an overlay default overrides the account default on this alias
        s.offered = BOTH.to_vec();
        s.overlay = Some(ServingOverlay {
            default_class: Some(ServingClass::Interactive),
            self_hosted_only: None,
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", &[]).await.status_code(), 200);
        assert_eq!(header(&mock.get_requests()[0], POOL_TAG_HEADER), Some("interactive"));
    }

    #[tokio::test]
    async fn no_policy_at_all_is_byte_identical_to_today() {
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(header(req, PRIORITY_HEADER), None);
    }

    #[tokio::test]
    async fn an_explicit_class_the_org_does_not_hold_or_the_model_does_not_offer_is_refused() {
        // not held
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:interactive", &[]).await;
        assert_eq!(response.status_code(), 403);
        let err: serde_json::Value = response.json();
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not available for this account"),
            "{err}"
        );
        assert!(mock.get_requests().is_empty(), "nothing reaches an upstream");

        // held, not offered
        s.offered = Vec::new();
        s.account = Some(holds(&BOTH));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:interactive", &[]).await;
        assert_eq!(response.status_code(), 403);
        let err: serde_json::Value = response.json();
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not available on model 'gpt-4'"),
            "{err}"
        );
        assert!(mock.get_requests().is_empty());
    }

    #[tokio::test]
    async fn inbound_envelope_headers_pass_through_and_a_resolution_overwrites_them() {
        // No policy: this crate is also the hop inside the serving namespace and
        // must forward what the first hop stamped; clients are scrubbed at dwctl.
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(
            &srv,
            "gpt-4",
            &[(POOL_TAG_HEADER, "interactive"), (PRIORITY_HEADER, "200")],
        )
        .await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), Some("interactive"), "second-hop pass-through");
        assert_eq!(header(req, PRIORITY_HEADER), Some("200"));

        s.account = Some(holds(&BOTH));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(
            &srv,
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
        let mut s = setup(&[DYNAMO]);
        s.offered = BOTH.to_vec();
        s.account = Some(holds(&BOTH));
        s.purpose = "batch";
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4:interactive", &[]).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(header(req, POOL_TAG_HEADER), None);
        assert_eq!(header(req, PRIORITY_HEADER), None);
        assert_eq!(body(req)["model"], "gpt-4");
    }

    #[tokio::test]
    async fn unknown_class_suffix_is_rejected_with_the_valid_set() {
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:fast", &[]).await;
        assert_eq!(response.status_code(), 400);
        let err: serde_json::Value = response.json();
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("'fast'"), "{message}");
        assert!(message.contains("interactive, throughput, standard"), "{message}");
        assert!(mock.get_requests().is_empty());
    }

    #[tokio::test]
    async fn self_hosted_only_accounts_never_reach_an_external_member() {
        let members = [DYNAMO, EXTERNAL];
        // Control: the self-hosted member fails and the pool falls over.
        let s = setup(&members);
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", &[]).await;
        assert_eq!(mock.get_requests().len(), 2, "without the setting both members are tried");

        let mut s = setup(&members);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4", &[]).await;
        assert!(response.status_code().is_server_error(), "{}", response.status_code());
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 1, "the external member is never attempted");
        assert!(requests[0].uri.starts_with("https://dynamo.example.com/"));

        // A hosted (non-dynamo, self-hosted) member is still eligible.
        let mut s = setup(&[DYNAMO, HOSTED]);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", &[]).await;
        assert_eq!(mock.get_requests().len(), 2);

        // A per-alias overlay can lift the account-wide restriction.
        let mut s = setup(&members);
        s.account = Some(AccountServing {
            self_hosted_only: true,
            ..Default::default()
        });
        s.overlay = Some(ServingOverlay {
            default_class: None,
            self_hosted_only: Some(false),
        });
        let mock = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);
        let srv = server(&s, &mock);
        post(&srv, "gpt-4", &[]).await;
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[test]
    fn accounts_and_overlays_round_trip_through_config_json() {
        let json = json!({
            "targets": {
                "gpt-4": {
                    "providers": [{"url": "https://dynamo.example.com/", "kind": "dynamo"}],
                    "serving_classes": ["interactive"],
                    "overlays": {"org-1": {"default_class": "interactive", "self_hosted_only": false}}
                }
            },
            "accounts": {"org-1": {"granted": ["interactive", "throughput"], "self_hosted_only": true}},
            "auth": {"global_keys": [], "key_definitions": {"k": {"key": "sk-1", "labels": {"account": "org-1"}}}}
        });
        let config: crate::target::ConfigFile = serde_json::from_value(json).unwrap();
        let targets = Targets::from_config(config).unwrap();
        let account = targets.accounts.get("org-1").unwrap();
        assert_eq!(account.granted, BOTH.to_vec());
        assert!(account.self_hosted_only);
        let pools = targets.targets.get("gpt-4").unwrap();
        assert_eq!(pools.active_serving_classes(), &[ServingClass::Interactive]);
        let overlay = pools.default_pool().overlays().get("org-1").unwrap();
        assert_eq!(overlay.default_class, Some(ServingClass::Interactive));
        assert_eq!(pools.default_pool().providers()[0].target.kind, ProviderKind::Dynamo);
        // A plain config still parses: no accounts, no overlays, kind defaults to external.
        let config: crate::target::ConfigFile =
            serde_json::from_value(json!({"targets": {"m": {"url": "https://x.example.com/"}}})).unwrap();
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
