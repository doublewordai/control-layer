//! Optional exact ingress synonyms. Primary names need no entries.
//!
//! Load once from the primary after catalog reconciliation and share the immutable
//! snapshot between ingress consumers. New spellings converge as pods roll. Do not
//! attach this small lookup to frequent key/auth reloads or use it for discovery.
//! Resolving a synonym is not admission: the live routing configuration still has
//! to accept its canonical model/class and authorize the authenticated account.

use std::{collections::HashMap, sync::Arc};

use sqlx::PgPool;

use crate::db::{errors::Result, handlers::model_aliases::ModelAliases, models::model_aliases::ModelAliasTarget};

/// Immutable, cheaply cloned snapshot of explicitly registered synonyms only.
#[derive(Clone, Debug, Default)]
pub struct ModelAliasMap {
    aliases: Arc<HashMap<String, ModelAliasTarget>>,
}

impl ModelAliasMap {
    /// Call after catalog reconciliation using the primary pool. No request-time
    /// database work, timers, notification subscriptions or per-key duplication.
    pub async fn load(primary: &PgPool) -> Result<Self> {
        let mut connection = primary.acquire().await?;
        let rows = ModelAliases::new(&mut connection).list_targets().await?;
        Ok(Self {
            aliases: Arc::new(rows.into_iter().map(|row| (row.alias.clone(), row)).collect()),
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = &ModelAliasTarget> {
        self.aliases.values()
    }

    /// Exact, case-sensitive lookup before interpreting a conventional suffix.
    /// A miss leaves ordinary model resolution to the live routing configuration;
    /// it does not reject a new primary model added since this snapshot loaded.
    /// Never chain lookups or guess alternative spellings/separators.
    pub fn resolve(&self, submitted: &str) -> Option<&ModelAliasTarget> {
        self.aliases.get(submitted)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClassRouteError {
    #[error("Model class is not configured")]
    UnknownClass,
    #[error("{0}")]
    Unavailable(&'static str),
}

/// Resolve an activated primary name or synonym against the live Onwards entry.
/// No database lookup and no access grant: Onwards still authenticates the key.
pub fn resolve_class_route(
    aliases: &ModelAliasMap,
    targets: &onwards::target::Targets,
    submitted: &str,
    asynchronous: bool,
) -> std::result::Result<Option<(String, onwards::serving::ClassRouteIdentity)>, ClassRouteError> {
    let synonym = aliases.resolve(submitted);
    let selected = match synonym {
        Some(alias) if alias.class_key == "standard" => alias.canonical_alias.clone(),
        Some(alias) => format!("{}:{}", alias.canonical_alias, alias.class_key),
        None => submitted.to_owned(),
    };
    let identity = targets
        .targets
        .get(&selected)
        .and_then(|p| p.default_pool().class_identity().cloned());
    let Some(mut identity) = identity else {
        if !targets.targets.contains_key(&selected)
            && let Some((base, _)) = selected.rsplit_once(':')
            && targets
                .targets
                .get(base)
                .is_some_and(|pool| pool.default_pool().class_identity().is_some())
        {
            return Err(ClassRouteError::UnknownClass);
        }
        return if synonym.is_some() {
            Err(ClassRouteError::Unavailable("Model alias is not active"))
        } else {
            Ok(None)
        };
    };
    if let Some(alias) = synonym
        && (identity.model_id != alias.deployed_model_id || identity.class_id != alias.serving_class_id)
    {
        return Err(ClassRouteError::Unavailable(
            "Model alias configuration changed; retry after rollout",
        ));
    }
    let selected = if asynchronous {
        let standard = targets
            .targets
            .get(&identity.canonical_alias)
            .and_then(|p| p.default_pool().class_identity().cloned())
            .filter(|c| c.model_id == identity.model_id && c.class_key == "standard")
            .ok_or(ClassRouteError::Unavailable("Standard model class is not available"))?;
        identity = standard;
        identity.canonical_alias.clone()
    } else {
        selected
    };
    Ok(Some((selected, identity)))
}
