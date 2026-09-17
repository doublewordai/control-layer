//! Serving classes: the per-request dispatch mode and how it is resolved.
//!
//! A serving class names how a request wants to be served. `interactive`
//! (per-stream speed, fast first token) and `throughput` (aggregate volume)
//! are *elevated* classes; `standard` is what everyone gets when nothing is
//! set. A class is a name for a **preset** of objective targets declared on
//! the model: a time-to-first-token target, an inter-token-latency target and
//! a scheduling priority. The serving stack's global router maps the targets
//! onto whichever pool of workers meets them, and its planner sizes those
//! pools from the traffic it sees; this crate never names a pool.
//!
//! Two facts gate an elevated class, both cheap to administer:
//!
//! * the **organisation holds it** — an account setting synced once per org
//!   ([`AccountServing::granted`]);
//! * the **model offers it** — a preset for that class declared on the alias
//!   ([`crate::target::PoolSpec::serving_classes`]).
//!
//! Resolution is one flat field:
//!
//! ```text
//! requested = model suffix, else the org's overlay default for this alias,
//!             else the org's account default, else none
//! resolved  = requested if the org holds it AND the alias offers it, else standard
//! targets   = the alias's preset for the resolved class (a `standard` preset
//!             is optional: without one, standard sends nothing)
//! ```
//!
//! An overlay may instead carry **explicit targets** for a bespoke deal on
//! one alias. Those are used as-is when the request names no class, imply the
//! authority to use them (no grant is checked) and resolve as [`ServingClass::Custom`].
//!
//! **Strict mode.** A class named on the request itself is a promise, never a
//! hint: if the org does not hold it, or the alias does not offer it, the
//! request is rejected rather than quietly served as `standard`. Only the
//! silent inputs (an account default, an overlay default) degrade silently,
//! because nobody asked for anything on that request.
//!
//! Daemon legs (batch and flex on the batch-purpose key, continuation resume
//! legs) always resolve to `standard` and send no targets: their existing
//! deadline priorities carry the ordering.
//!
//! The targets travel in the request body as `nvext.router.{ttft_target,
//! itl_target}` and, when the preset carries one, the priority as
//! `nvext.agent_hints.priority`, on members of kind `dynamo` only. Everything
//! a client could send to steer this itself is scrubbed at dwctl's ingress;
//! only the resolver sets them.

use std::collections::BTreeMap;
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
/// Separator between a model alias and its class suffix (`alias:class`).
pub const SUFFIX_SEPARATOR: char = ':';
/// Key label naming the account (organisation) that owns the key. The account
/// policy in [`crate::target::Targets::accounts`] and an alias's overlays are
/// both keyed by its value.
pub const ACCOUNT_LABEL: &str = "account";

/// The dispatch mode a request is served under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServingClass {
    /// Elevated: per-stream speed.
    Interactive,
    /// Elevated: aggregate volume.
    Throughput,
    /// The absence of a choice.
    Standard,
    /// Explicit targets from an overlay: a bespoke deal on one alias. Never
    /// requestable by suffix, never declared on a model or an account: an
    /// outcome only, so configuration cannot spell it.
    #[serde(skip_deserializing)]
    Custom,
}

impl ServingClass {
    /// Every class a request may name, in the order the 400 message lists them.
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
            ServingClass::Custom => "custom",
        }
    }

    /// Elevated classes are the ones a model offers and an org holds.
    pub fn is_elevated(self) -> bool {
        matches!(self, ServingClass::Interactive | ServingClass::Throughput)
    }
}

impl fmt::Display for ServingClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A class name that is not in the requestable set.
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
pub fn split_class_suffix(
    model: &str,
) -> Result<(&str, Option<ServingClass>), UnknownServingClass> {
    match model.rsplit_once(SUFFIX_SEPARATOR) {
        None => Ok((model, None)),
        Some((alias, suffix)) => suffix.parse().map(|class| (alias, Some(class))),
    }
}

/// The objective targets one request is served to. A class is a name for a
/// preset of these on a model; an overlay may carry them explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingTargets {
    /// Time-to-first-token target, milliseconds.
    pub ttft_ms: u32,
    /// Inter-token-latency target, milliseconds.
    pub itl_ms: u32,
    /// Scheduling priority on the serving stack (higher wins; 0 is the
    /// implicit realtime value and is not sent).
    #[serde(default)]
    pub priority: i32,
}

impl ServingTargets {
    /// Write the targets into a request body: `nvext.router.{ttft_target,
    /// itl_target}` always, `nvext.agent_hints.priority` only when non-zero.
    /// Existing values are overwritten; the rest of `nvext` is kept.
    pub fn stamp(&self, body: &mut serde_json::Map<String, serde_json::Value>) {
        let nvext = body
            .entry(NVEXT_FIELD)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if !nvext.is_object() {
            *nvext = serde_json::Value::Object(Default::default());
        }
        let nvext = nvext
            .as_object_mut()
            .expect("nvext was just made an object");
        let router = nvext
            .entry(NVEXT_ROUTER_FIELD)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if !router.is_object() {
            *router = serde_json::Value::Object(Default::default());
        }
        let router = router
            .as_object_mut()
            .expect("router was just made an object");
        router.insert(TTFT_TARGET_FIELD.to_string(), self.ttft_ms.into());
        router.insert(ITL_TARGET_FIELD.to_string(), self.itl_ms.into());
        if self.priority != 0 {
            let hints = nvext
                .entry(NVEXT_AGENT_HINTS_FIELD)
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if !hints.is_object() {
                *hints = serde_json::Value::Object(Default::default());
            }
            hints
                .as_object_mut()
                .expect("agent_hints was just made an object")
                .insert(PRIORITY_FIELD.to_string(), self.priority.into());
        }
    }
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
        router.remove(TTFT_TARGET_FIELD),
        router.remove(ITL_TARGET_FIELD),
    ];
    if router.is_empty() {
        nvext.remove(NVEXT_ROUTER_FIELD);
    }
    removed.iter().any(Option::is_some)
}

/// The presets an alias offers, by class. Declaring a preset for a class is
/// what makes the alias offer it; a `standard` preset is optional and only
/// changes what `standard` sends.
pub type ServingPresets = BTreeMap<ServingClass, ServingTargets>;

/// What kind of server a provider is. Decides who receives the targets and
/// who counts as external for a self-hosted-only organisation. Deliberately
/// its own fact rather than a reading of some other flag (`trusted`,
/// `accepts_scheduling_priority`): those correlate with it today and mean
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Self-hosted, behind the dynamo frontend. The only kind that receives
    /// the serving targets.
    Dynamo,
    /// Self-hosted, not behind dynamo.
    Hosted,
    /// A third-party provider. Never receives the targets; skipped for
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
/// the alias's pool spec, keyed by account id. `default_class` and `targets`
/// are mutually exclusive: the catalog refuses both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingOverlay {
    /// Overrides the account's default class on this alias.
    #[serde(default)]
    pub default_class: Option<ServingClass>,
    /// Explicit targets for a bespoke deal on this alias, used when the
    /// request names no class. Writing them implies authority: no grant is
    /// checked, and they take precedence over any preset.
    #[serde(default)]
    pub targets: Option<ServingTargets>,
    /// Overrides the account's `self_hosted_only` on this alias.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
}

/// Key purposes whose requests are daemon legs: the ordering already travels
/// as a deadline-derived (batch, flex) or fixed (continuation resume)
/// priority in the body, and no targets are sent.
const DAEMON_PURPOSES: [&str; 2] = ["batch", "continuation"];

/// The outcome of resolving one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingResolution {
    pub requested: Option<ServingClass>,
    pub resolved: ServingClass,
    /// What a `dynamo` member is told to serve to; `None` sends nothing.
    pub targets: Option<ServingTargets>,
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
                write!(
                    f,
                    "Serving class '{class}' is not available for this account."
                )
            }
            ClassRejection::NotOffered { class, alias } => {
                write!(
                    f,
                    "Serving class '{class}' is not available on model '{alias}'."
                )
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
/// * `presets` — the classes the alias offers, each with its targets.
/// * `purpose` — the key's purpose label; daemon purposes resolve to standard.
pub fn resolve(
    suffix: Option<ServingClass>,
    account: Option<&AccountServing>,
    overlay: Option<&ServingOverlay>,
    alias: &str,
    presets: &ServingPresets,
    purpose: Option<&str>,
) -> Result<ServingResolution, ClassRejection> {
    let self_hosted_only = overlay
        .and_then(|o| o.self_hosted_only)
        .unwrap_or_else(|| account.is_some_and(|a| a.self_hosted_only));

    if purpose.is_some_and(|p| DAEMON_PURPOSES.contains(&p)) {
        return Ok(ServingResolution {
            requested: None,
            resolved: ServingClass::Standard,
            targets: None,
            self_hosted_only,
        });
    }

    let held = |class: ServingClass| account.is_some_and(|a| a.granted.contains(&class));
    let offered = |class: ServingClass| presets.contains_key(&class);

    // Strict mode: a class named on the request is honoured or refused.
    if let Some(class) = suffix
        && class.is_elevated()
    {
        if !held(class) {
            return Err(ClassRejection::NotHeld { class });
        }
        if !offered(class) {
            return Err(ClassRejection::NotOffered {
                class,
                alias: alias.to_string(),
            });
        }
    }

    // Explicit overlay targets: the bespoke deal applies whenever the
    // request itself names nothing. A suffix still outranks them.
    if suffix.is_none()
        && let Some(targets) = overlay.and_then(|o| o.targets)
    {
        return Ok(ServingResolution {
            requested: None,
            resolved: ServingClass::Custom,
            targets: Some(targets),
            self_hosted_only,
        });
    }

    let requested = suffix
        .or(overlay.and_then(|o| o.default_class))
        .or(account.and_then(|a| a.default_class));

    let resolved = match requested {
        Some(class) if class.is_elevated() && held(class) && offered(class) => class,
        _ => ServingClass::Standard,
    };

    Ok(ServingResolution {
        requested,
        resolved,
        targets: presets.get(&resolved).copied(),
        self_hosted_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERACTIVE: ServingTargets = ServingTargets {
        ttft_ms: 500,
        itl_ms: 20,
        priority: 200,
    };
    const THROUGHPUT: ServingTargets = ServingTargets {
        ttft_ms: 5_000,
        itl_ms: 100,
        priority: 100,
    };
    const BOTH_CLASSES: [ServingClass; 2] = [ServingClass::Interactive, ServingClass::Throughput];

    fn both() -> ServingPresets {
        ServingPresets::from([
            (ServingClass::Interactive, INTERACTIVE),
            (ServingClass::Throughput, THROUGHPUT),
        ])
    }

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
        presets: &ServingPresets,
    ) -> Result<ServingResolution, ClassRejection> {
        resolve(suffix, account, overlay, "m", presets, Some("realtime"))
    }

    #[test]
    fn suffix_is_split_and_unknown_class_is_an_error() {
        assert_eq!(
            split_class_suffix("zai-org/GLM-5.2"),
            Ok(("zai-org/GLM-5.2", None))
        );
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
        assert!(
            err.to_string()
                .contains("interactive, throughput, standard")
        );
        // `custom` is an outcome, never a request
        assert!(split_class_suffix("m:custom").is_err());
    }

    #[test]
    fn nothing_set_resolves_to_standard_with_no_request_and_no_targets() {
        let r = realtime(None, None, None, &both()).unwrap();
        assert_eq!(r.requested, None);
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(r.targets, None);
        assert!(!r.self_hosted_only);
    }

    #[test]
    fn precedence_is_suffix_then_overlay_then_account() {
        let a = account(&BOTH_CLASSES, Some(ServingClass::Interactive));
        let o = ServingOverlay {
            default_class: Some(ServingClass::Throughput),
            ..Default::default()
        };
        // suffix wins over everything
        let r = realtime(Some(ServingClass::Interactive), Some(&a), Some(&o), &both()).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
        assert_eq!(r.targets, Some(INTERACTIVE));
        // overlay default wins over the account default
        let r = realtime(None, Some(&a), Some(&o), &both()).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Throughput));
        assert_eq!(r.resolved, ServingClass::Throughput);
        assert_eq!(r.targets, Some(THROUGHPUT));
        // account default alone
        let r = realtime(None, Some(&a), None, &both()).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn explicit_overlay_targets_apply_without_a_grant_and_lose_to_a_suffix() {
        let bespoke = ServingTargets {
            ttft_ms: 800,
            itl_ms: 30,
            priority: 0,
        };
        let o = ServingOverlay {
            targets: Some(bespoke),
            ..Default::default()
        };
        // no account settings at all, and a model that offers nothing: still applied
        let r = realtime(None, None, Some(&o), &ServingPresets::new()).unwrap();
        assert_eq!(r.requested, None);
        assert_eq!(r.resolved, ServingClass::Custom);
        assert_eq!(r.targets, Some(bespoke));
        // an explicit suffix outranks the deal, and is still gated
        let a = account(&BOTH_CLASSES, None);
        let r = realtime(Some(ServingClass::Throughput), Some(&a), Some(&o), &both()).unwrap();
        assert_eq!(r.resolved, ServingClass::Throughput);
        assert_eq!(r.targets, Some(THROUGHPUT));
        let r = realtime(Some(ServingClass::Standard), Some(&a), Some(&o), &both()).unwrap();
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(r.targets, None);
        assert!(realtime(Some(ServingClass::Interactive), None, Some(&o), &both()).is_err());
        // daemon legs ignore the deal too
        let r = resolve(None, None, Some(&o), "m", &both(), Some("batch")).unwrap();
        assert_eq!(r.targets, None);
    }

    #[test]
    fn explicit_standard_suffix_opts_down_and_sends_the_standard_preset_if_any() {
        let a = account(&BOTH_CLASSES, Some(ServingClass::Interactive));
        let r = realtime(Some(ServingClass::Standard), Some(&a), None, &both()).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Standard));
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(
            r.targets, None,
            "no standard preset: standard sends nothing"
        );

        let standard = ServingTargets {
            ttft_ms: 10_000,
            itl_ms: 200,
            priority: 0,
        };
        let mut presets = both();
        presets.insert(ServingClass::Standard, standard);
        let r = realtime(Some(ServingClass::Standard), Some(&a), None, &presets).unwrap();
        assert_eq!(r.targets, Some(standard));
        let r = realtime(None, None, None, &presets).unwrap();
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(
            r.targets,
            Some(standard),
            "the model pins what standard means"
        );
    }

    #[test]
    fn strict_mode_refuses_an_explicit_class_the_org_does_not_hold_or_the_model_does_not_offer() {
        // not held (no account at all, or account without it)
        let err = realtime(Some(ServingClass::Interactive), None, None, &both()).unwrap_err();
        assert_eq!(
            err,
            ClassRejection::NotHeld {
                class: ServingClass::Interactive
            }
        );
        let a = account(&[ServingClass::Throughput], None);
        let err = realtime(Some(ServingClass::Interactive), Some(&a), None, &both()).unwrap_err();
        assert!(matches!(err, ClassRejection::NotHeld { .. }));
        assert!(err.to_string().contains("not available for this account"));
        // held but the model does not offer it
        let a = account(&BOTH_CLASSES, None);
        let err = realtime(
            Some(ServingClass::Interactive),
            Some(&a),
            None,
            &ServingPresets::new(),
        )
        .unwrap_err();
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
        let a = account(&BOTH_CLASSES, Some(ServingClass::Interactive));
        let r = realtime(None, Some(&a), None, &ServingPresets::new()).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Interactive));
        assert_eq!(r.resolved, ServingClass::Standard);
        assert_eq!(r.targets, None);
        // overlay default for a class the org does not hold: same
        let a = account(&[], None);
        let o = ServingOverlay {
            default_class: Some(ServingClass::Throughput),
            ..Default::default()
        };
        let r = realtime(None, Some(&a), Some(&o), &both()).unwrap();
        assert_eq!(r.requested, Some(ServingClass::Throughput));
        assert_eq!(r.resolved, ServingClass::Standard);
    }

    #[test]
    fn daemon_legs_always_resolve_to_standard_even_with_a_suffix() {
        let a = account(&BOTH_CLASSES, Some(ServingClass::Interactive));
        for purpose in DAEMON_PURPOSES {
            let r = resolve(
                Some(ServingClass::Interactive),
                Some(&a),
                None,
                "m",
                &both(),
                Some(purpose),
            )
            .unwrap();
            assert_eq!(r.requested, None, "{purpose}");
            assert_eq!(r.resolved, ServingClass::Standard, "{purpose}");
            assert_eq!(r.targets, None, "{purpose}");
        }
        // playground behaves like realtime
        let r = resolve(None, Some(&a), None, "m", &both(), Some("playground")).unwrap();
        assert_eq!(r.resolved, ServingClass::Interactive);
    }

    #[test]
    fn self_hosted_only_comes_from_the_account_unless_the_overlay_overrides() {
        let mut a = account(&[], None);
        a.self_hosted_only = true;
        let none = ServingPresets::new();
        assert!(
            realtime(None, Some(&a), None, &none)
                .unwrap()
                .self_hosted_only
        );
        // the daemon leg carries the restriction too: batch work must not spill either
        assert!(
            resolve(None, Some(&a), None, "m", &none, Some("batch"))
                .unwrap()
                .self_hosted_only
        );
        let o = ServingOverlay {
            self_hosted_only: Some(false),
            ..Default::default()
        };
        assert!(
            !realtime(None, Some(&a), Some(&o), &none)
                .unwrap()
                .self_hosted_only
        );
    }

    #[test]
    fn targets_are_stamped_into_nvext_and_scrubbed_from_it() {
        let mut body = serde_json::json!({
            "model": "m",
            "nvext": {"cache_control": {"enabled": true}, "router": {"ttft_target": 1}, "agent_hints": {"max_batch_size": 8}}
        });
        INTERACTIVE.stamp(body.as_object_mut().unwrap());
        assert_eq!(body["nvext"]["router"]["ttft_target"], 500, "overwritten");
        assert_eq!(body["nvext"]["router"]["itl_target"], 20);
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 200);
        assert_eq!(
            body["nvext"]["agent_hints"]["max_batch_size"], 8,
            "the rest of the hints survive"
        );
        assert_eq!(body["nvext"]["cache_control"]["enabled"], true);

        // priority 0 sends no priority at all; a body with no nvext gets one
        let mut body = serde_json::json!({"model": "m"});
        ServingTargets {
            ttft_ms: 1,
            itl_ms: 2,
            priority: 0,
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
    fn kinds_and_classes() {
        assert!(ServingClass::Interactive.is_elevated());
        assert!(!ServingClass::Standard.is_elevated());
        assert!(!ServingClass::Custom.is_elevated());
        assert!(AccountServing::default().is_empty());
        assert_eq!(ProviderKind::default(), ProviderKind::External);
        assert_eq!("dynamo".parse::<ProviderKind>(), Ok(ProviderKind::Dynamo));
        assert!("cloud".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn serde_uses_lowercase_names_and_class_keyed_presets() {
        let json = serde_json::to_string(&AccountServing {
            granted: vec![ServingClass::Interactive],
            ..Default::default()
        })
        .unwrap();
        assert!(json.contains("\"granted\":[\"interactive\"]"));
        let back: AccountServing = serde_json::from_str(&json).unwrap();
        assert_eq!(back.granted, vec![ServingClass::Interactive]);
        assert_eq!(
            serde_json::to_string(&ProviderKind::Dynamo).unwrap(),
            "\"dynamo\""
        );

        // `custom` is an outcome, never configuration.
        assert_eq!(
            serde_json::to_string(&ServingClass::Custom).unwrap(),
            "\"custom\""
        );
        assert!(serde_json::from_str::<ServingClass>("\"custom\"").is_err());
        assert!(serde_json::from_str::<AccountServing>(r#"{"granted": ["custom"]}"#).is_err());
        assert!(
            serde_json::from_str::<ServingPresets>(r#"{"custom": {"ttft_ms": 1, "itl_ms": 1}}"#)
                .is_err()
        );

        let presets: ServingPresets =
            serde_json::from_str(r#"{"interactive": {"ttft_ms": 500, "itl_ms": 20, "priority": 200}, "standard": {"ttft_ms": 9, "itl_ms": 9}}"#)
                .unwrap();
        assert_eq!(presets[&ServingClass::Interactive], INTERACTIVE);
        assert_eq!(
            presets[&ServingClass::Standard].priority,
            0,
            "priority defaults to 0"
        );
        assert_eq!(
            serde_json::to_value(&presets).unwrap()["interactive"]["ttft_ms"],
            500
        );
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
    const OK_BODY: &str =
        r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4","choices":[]}"#;

    /// A member of the composite: `(url, kind)`.
    type Member = (&'static str, ProviderKind);

    struct Setup {
        members: Vec<Member>,
        presets: ServingPresets,
        purpose: &'static str,
        account: Option<AccountServing>,
        overlay: Option<ServingOverlay>,
    }

    fn setup(members: &[Member]) -> Setup {
        Setup {
            members: members.to_vec(),
            presets: ServingPresets::new(),
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
        .with_serving(s.presets.clone(), overlays);
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

    fn holds(classes: &[ServingClass]) -> AccountServing {
        AccountServing {
            granted: classes.to_vec(),
            ..Default::default()
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

    const INTERACTIVE: ServingTargets = ServingTargets {
        ttft_ms: 500,
        itl_ms: 20,
        priority: 200,
    };
    const THROUGHPUT: ServingTargets = ServingTargets {
        ttft_ms: 5_000,
        itl_ms: 100,
        priority: 0,
    };
    const BOTH_CLASSES: [ServingClass; 2] = [ServingClass::Interactive, ServingClass::Throughput];
    fn both() -> ServingPresets {
        ServingPresets::from([
            (ServingClass::Interactive, INTERACTIVE),
            (ServingClass::Throughput, THROUGHPUT),
        ])
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
    async fn elevated_class_sends_its_preset_to_a_dynamo_member_and_the_suffix_is_stripped() {
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        s.account = Some(holds(&BOTH_CLASSES));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);

        let response = post(&srv, "gpt-4:interactive", json!({})).await;
        assert_eq!(response.status_code(), 200);
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(sent(req), Some((500, 20, Some(200))));
        assert_eq!(
            body(req)["model"],
            "gpt-4",
            "the suffix never reaches an upstream"
        );
        assert_eq!(
            body(req)["messages"][0]["content"],
            "hi",
            "the rest of the body is untouched"
        );

        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(
            post(&srv, "gpt-4:throughput", json!({}))
                .await
                .status_code(),
            200
        );
        let req = &mock.get_requests()[0];
        assert_eq!(
            sent(req),
            Some((5_000, 100, None)),
            "a zero priority is not sent"
        );
    }

    #[tokio::test]
    async fn only_dynamo_members_receive_the_targets() {
        for member in [EXTERNAL, HOSTED] {
            let mut s = setup(&[member]);
            s.presets = both();
            s.account = Some(holds(&BOTH_CLASSES));
            let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
            let srv = server(&s, &mock);
            assert_eq!(
                post(&srv, "gpt-4:interactive", json!({}))
                    .await
                    .status_code(),
                200
            );
            let req = &mock.get_requests()[0];
            assert_eq!(sent(req), None, "{:?}", member.1);
            assert!(body(req).get("nvext").is_none(), "{:?}", member.1);
            assert_eq!(body(req)["model"], "gpt-4");
        }
    }

    #[tokio::test]
    async fn account_default_applies_silently_and_degrades_silently() {
        // held and offered: the default elevates without a suffix
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        s.account = Some(AccountServing {
            granted: BOTH_CLASSES.to_vec(),
            default_class: Some(ServingClass::Throughput),
            self_hosted_only: false,
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", json!({})).await.status_code(), 200);
        assert_eq!(sent(&mock.get_requests()[0]), Some((5_000, 100, None)));

        // same account, a model that offers nothing: served, nothing sent
        s.presets = ServingPresets::new();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", json!({})).await.status_code(), 200);
        assert_eq!(sent(&mock.get_requests()[0]), None);

        // an overlay default overrides the account default on this alias
        s.presets = both();
        s.overlay = Some(ServingOverlay {
            default_class: Some(ServingClass::Interactive),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", json!({})).await.status_code(), 200);
        assert_eq!(sent(&mock.get_requests()[0]), Some((500, 20, Some(200))));
    }

    #[tokio::test]
    async fn explicit_overlay_targets_are_sent_as_is() {
        let mut s = setup(&[DYNAMO]);
        s.overlay = Some(ServingOverlay {
            targets: Some(ServingTargets {
                ttft_ms: 800,
                itl_ms: 30,
                priority: 50,
            }),
            ..Default::default()
        });
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", json!({})).await.status_code(), 200);
        assert_eq!(sent(&mock.get_requests()[0]), Some((800, 30, Some(50))));
    }

    #[tokio::test]
    async fn no_policy_at_all_is_byte_identical_to_today() {
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        assert_eq!(post(&srv, "gpt-4", json!({})).await.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert!(body(req).get("nvext").is_none());
    }

    #[tokio::test]
    async fn an_explicit_class_the_org_does_not_hold_or_the_model_does_not_offer_is_refused() {
        // not held
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:interactive", json!({})).await;
        assert_eq!(response.status_code(), 403);
        let err: serde_json::Value = response.json();
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not available for this account"),
            "{err}"
        );
        assert!(
            mock.get_requests().is_empty(),
            "nothing reaches an upstream"
        );

        // held, not offered
        s.presets = ServingPresets::new();
        s.account = Some(holds(&BOTH_CLASSES));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:interactive", json!({})).await;
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
    async fn inbound_targets_pass_through_and_a_resolution_overwrites_them() {
        // No policy: this crate is also the hop inside the serving namespace and
        // must forward what the first hop stamped; clients are scrubbed at dwctl.
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let inbound = json!({"nvext": {"router": {"ttft_target": 500, "itl_target": 20}, "agent_hints": {"priority": 200}}});
        let response = post(&srv, "gpt-4", inbound.clone()).await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(
            sent(req),
            Some((500, 20, Some(200))),
            "second-hop pass-through"
        );

        s.account = Some(holds(&BOTH_CLASSES));
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:throughput", inbound).await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(
            sent(req),
            Some((5_000, 100, Some(200))),
            "the targets are overwritten"
        );
    }

    #[tokio::test]
    async fn daemon_purpose_keys_resolve_to_standard_even_with_a_suffix() {
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        s.account = Some(holds(&BOTH_CLASSES));
        s.purpose = "batch";
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        // the deadline priority the daemon injected travels untouched
        let response = post(
            &srv,
            "gpt-4:interactive",
            json!({"nvext": {"agent_hints": {"priority": -1234}}}),
        )
        .await;
        assert_eq!(response.status_code(), 200);
        let req = &mock.get_requests()[0];
        assert_eq!(sent(req), None);
        assert_eq!(body(req)["nvext"]["agent_hints"]["priority"], -1234);
        assert_eq!(body(req)["model"], "gpt-4");
    }

    #[tokio::test]
    async fn an_upstream_failure_still_reports_the_resolved_class() {
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        s.account = Some(holds(&BOTH_CLASSES));
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
                json!({"model": "gpt-4:interactive", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
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
        assert_eq!(outcome.requested, Some(ServingClass::Interactive));
        assert_eq!(outcome.resolved, ServingClass::Interactive);
    }

    #[tokio::test]
    async fn unknown_class_suffix_is_rejected_with_the_valid_set() {
        let s = setup(&[DYNAMO]);
        let mock = MockHttpClient::new(StatusCode::OK, OK_BODY);
        let srv = server(&s, &mock);
        let response = post(&srv, "gpt-4:fast", json!({})).await;
        assert_eq!(response.status_code(), 400);
        let err: serde_json::Value = response.json();
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("'fast'"), "{message}");
        assert!(
            message.contains("interactive, throughput, standard"),
            "{message}"
        );
        assert!(mock.get_requests().is_empty());
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
        // Presets and overlays are declared on the alias's default pool; a
        // request served by a named pool (here: completions) must still see
        // them, or an explicit class would be refused as "not offered".
        let mut s = setup(&[DYNAMO]);
        s.presets = both();
        s.account = Some(holds(&BOTH_CLASSES));
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
            .json(&json!({"model": "gpt-4:interactive", "prompt": "hi"}))
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

    #[test]
    fn accounts_and_overlays_round_trip_through_config_json() {
        let json = json!({
            "targets": {
                "gpt-4": {
                    "providers": [{"url": "https://dynamo.example.com/", "kind": "dynamo"}],
                    "serving_classes": {"interactive": {"ttft_ms": 500, "itl_ms": 20, "priority": 200}},
                    "overlays": {
                        "org-1": {"default_class": "interactive", "self_hosted_only": false},
                        "org-2": {"targets": {"ttft_ms": 800, "itl_ms": 30}}
                    }
                }
            },
            "accounts": {"org-1": {"granted": ["interactive", "throughput"], "self_hosted_only": true}},
            "auth": {"global_keys": [], "key_definitions": {"k": {"key": "sk-1", "labels": {"account": "org-1"}}}}
        });
        let config: crate::target::ConfigFile = serde_json::from_value(json).unwrap();
        let targets = Targets::from_config(config).unwrap();
        let account = targets.accounts.get("org-1").unwrap();
        assert_eq!(account.granted, BOTH_CLASSES.to_vec());
        assert!(account.self_hosted_only);
        let pools = targets.targets.get("gpt-4").unwrap();
        assert_eq!(
            pools.active_serving_classes()[&ServingClass::Interactive],
            INTERACTIVE
        );
        let overlays = pools.default_pool().overlays();
        assert_eq!(
            overlays["org-1"].default_class,
            Some(ServingClass::Interactive)
        );
        assert_eq!(
            overlays["org-2"]
                .targets
                .map(|t| (t.ttft_ms, t.itl_ms, t.priority)),
            Some((800, 30, 0))
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
