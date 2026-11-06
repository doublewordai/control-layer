//! The model a request names, split once into its model and serving class.
//!
//! A request names `model` (the model's standard class) or `model:<class>`, a
//! class route the gateway configures for a class-routed model. Model aliases
//! never contain `:`, so the first colon is always the class separator.
//!
//! dwctl's inference middleware resolves the name once and attaches a
//! [`ServingName`] to the request. Properties of the model (tokenization,
//! access, reasoning efforts, modality) use [`ServingName::model`];
//! destinations (routing, limits, prices, cache scope, analytics, metrics) use
//! the model with [`ServingName::class_label`].

use axum::http::Extensions;

use super::rejections::Recorder;

/// The class a request without a class suffix is served by.
pub const STANDARD_CLASS: &str = "standard";

/// Separator between a model alias and a class (`model:<class>`).
pub const CLASS_SEPARATOR: char = ':';

/// The model and class a request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServingName {
    /// The name as sent.
    pub requested: String,
    /// The model alias.
    pub model: String,
    /// The class a `model:<class>` route names; `None` for the standard class.
    pub class: Option<String>,
}

impl ServingName {
    /// Split `requested` using the gateway's class routes: only a configured
    /// class route names a class. Anything else is a model name as sent.
    pub fn resolve(requested: &str, recorder: Option<&Recorder>) -> Self {
        if let Some(model) = recorder.and_then(|recorder| recorder.class_route_of(requested))
            && let Some(class) = requested
                .strip_prefix(model.as_str())
                .and_then(|rest| rest.strip_prefix(CLASS_SEPARATOR))
        {
            return Self {
                requested: requested.to_owned(),
                class: Some(class.to_owned()),
                model,
            };
        }
        Self {
            requested: requested.to_owned(),
            model: requested.to_owned(),
            class: None,
        }
    }

    /// The name the inference middleware attached for `requested`, or a fresh
    /// resolution when it did not run (e.g. dispatched batch and flex work).
    pub fn of(extensions: &Extensions, requested: &str) -> Self {
        match extensions.get::<ServingName>() {
            Some(name) if name.requested == requested => name.clone(),
            _ => Self::resolve(requested, extensions.get::<Recorder>()),
        }
    }

    /// The class as a label value: `standard` unless a class route was named.
    pub fn class_label(&self) -> &str {
        self.class.as_deref().unwrap_or(STANDARD_CLASS)
    }
}

/// Split the name a served request named into its model and class. Only a
/// class route can be served under a name with a `:` (aliases never contain
/// one, and an unknown suffix is never served), so a served name splits at its
/// colon. For a request that may not have been served, use [`ServingName::resolve`].
pub fn split_served(requested: &str) -> (&str, Option<&str>) {
    match requested.split_once(CLASS_SEPARATOR) {
        Some((model, class)) => (model, Some(class)),
        None => (requested, None),
    }
}

/// Whether `model` carries a class suffix. Flex, background and batch work
/// always runs on a model's standard class, so such names are refused there.
pub fn has_class_suffix(model: &str) -> bool {
    model.contains(CLASS_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use onwards::target::Targets;

    use super::*;

    fn recorder() -> Recorder {
        Recorder::new(
            Targets::from_config(
                serde_json::from_value(serde_json::json!({
                    "targets": {
                        "org/model": {"providers": [{"url": "https://upstream.example.com/v1/"}]},
                        "org/model:fast": {"providers": [{"url": "https://upstream.example.com/v1/"}], "class_route_of": "org/model"}
                    },
                    "auth": {"global_keys": [], "key_definitions": {}}
                }))
                .unwrap(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn only_a_configured_class_route_names_a_class() {
        let recorder = recorder();
        let fast = ServingName::resolve("org/model:fast", Some(&recorder));
        assert_eq!((fast.model.as_str(), fast.class_label()), ("org/model", "fast"));
        let standard = ServingName::resolve("org/model", Some(&recorder));
        assert_eq!(
            (standard.model.as_str(), standard.class.as_deref(), standard.class_label()),
            ("org/model", None, "standard")
        );
        // An undeclared class is not split: it names no model and is refused downstream.
        let unknown = ServingName::resolve("org/model:gibberish", Some(&recorder));
        assert_eq!((unknown.model.as_str(), unknown.class), ("org/model:gibberish", None));
    }

    #[test]
    fn served_names_split_at_the_colon() {
        assert_eq!(split_served("org/model:fast"), ("org/model", Some("fast")));
        assert_eq!(split_served("org/model"), ("org/model", None));
    }

    #[test]
    fn suffixes_are_detected() {
        assert!(has_class_suffix("org/model:fast"));
        assert!(!has_class_suffix("org/model"));
    }
}
