//! Mid-stream continuation: resume generations that die mid-stream.
//!
//! When an upstream kills a stream part-way through a generation, the resume
//! middleware (wired inner to outlet/cache, outer to error enrichment) rebuilds
//! the exact prompt + partial output as a token-id vector via tokenizer-svc
//! `/v1/render` and re-enters the onwards stack as a `/v1/completions` request
//! on the model's continuation composite (dynamo first, provider fallback).
//! The client keeps one uninterrupted stream; outlet/billing see one logical
//! request with a merged usage frame.
//!
//! This module currently owns the **global continuation key**: a single hidden
//! `continuation`-purpose API key that authenticates resume legs into onwards
//! and carries the purpose label the `model_traffic_rules` redirect fires on.
//! It is deliberately global (not per-user):
//! - resume legs must keep working even when the requesting user's own keys
//!   have been pulled mid-stream (e.g. credit exhaustion) — once we have
//!   accepted and partially streamed a response, we finish it; the user is
//!   still billed normally via the merged frame on the original request;
//! - key cardinality stays constant as users grow (onwards config sync cost
//!   scales with key count);
//! - resumes are model/provider faults, so throttling belongs per-model in the
//!   middleware, not per-user on the key.
//!
//! The key is owned by the initial admin user and provisioned at startup so it
//! has synced into the onwards key cache before the first resume attempt.

pub mod accumulate;
pub mod detect;
pub mod metrics;
pub mod render;
pub mod rewrap;

use sqlx::PgPool;

use crate::UserId;
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;

/// Get or create the global hidden continuation key, returning its secret.
///
/// Idempotent (`ON CONFLICT` upsert keyed on owner + purpose): the startup call
/// guarantees existence/sync, and the resume middleware calls it again to
/// obtain the secret without caring which call created the row.
pub async fn provision_global_key(pool: &PgPool, admin_user_id: UserId) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let secret = ApiKeys::new(&mut tx)
        .get_or_create_hidden_key(admin_user_id, ApiKeyPurpose::Continuation, admin_user_id)
        .await?;
    tx.commit().await?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::create_test_user;

    /// Provisioning twice returns the same secret (idempotent upsert), and the
    /// row is a hidden continuation-purpose key owned by the given user.
    #[sqlx::test]
    async fn provision_global_key_is_idempotent_and_hidden(pool: PgPool) {
        let admin = create_test_user(&pool, Role::PlatformManager).await;

        let first = provision_global_key(&pool, admin.id).await.unwrap();
        let second = provision_global_key(&pool, admin.id).await.unwrap();
        assert_eq!(first, second);

        let row = sqlx::query!(r#"SELECT user_id, purpose, hidden FROM api_keys WHERE secret = $1"#, first)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.user_id, admin.id);
        assert_eq!(row.purpose, "continuation");
        assert!(row.hidden);
    }
}
