//! Account-level feature checks shared with background SQL through `user_has_feature`.
//!
//! Callers choose the owning account explicitly; organization membership never
//! implicitly grants an organization's features to personal API keys.

use crate::{db::errors::Result, types::UserId};
use sqlx::PgConnection;

/// Supported account features. Adding a flag does not require a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureFlag {
    /// Permit debt while continuing to record usage charges and enforce key caps.
    AllowNegativeBalance,
}

impl FeatureFlag {
    /// Stable database name, also used by SQL-only consumers.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowNegativeBalance => "ALLOW_NEGATIVE_BALANCE",
        }
    }
}

/// Read-only feature access. Operators manage flags directly in the database.
pub struct FeatureFlags<'c> {
    db: &'c mut PgConnection,
}

impl<'c> FeatureFlags<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Missing, disabled, and soft-deleted accounts have no enabled features.
    pub async fn has_feature(&mut self, account_id: UserId, feature: FeatureFlag) -> Result<bool> {
        Ok(
            sqlx::query_scalar!(r#"SELECT user_has_feature($1, $2) AS "enabled!""#, account_id, feature.as_str(),)
                .fetch_one(&mut *self.db)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    use tokio::time::{Duration, timeout};

    #[sqlx::test(fixtures(path = "../../sync/onwards_config/fixtures", scripts("cache_base")))]
    async fn test_feature_flags_lifecycle(pool: PgPool) {
        let account: uuid::Uuid = "00000000-0000-0000-0000-0000000000a1".parse().unwrap();
        let other: uuid::Uuid = "00000000-0000-0000-0000-0000000000b1".parse().unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let feature = FeatureFlag::AllowNegativeBalance;
        assert!(!FeatureFlags::new(&mut conn).has_feature(account, feature).await.unwrap());
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await.unwrap();
        listener.listen("auth_config_changed").await.unwrap();

        for (sql, expected) in [
            (
                "INSERT INTO user_feature_flags (user_id, feature_flag) VALUES ($1, 'ALLOW_NEGATIVE_BALANCE')",
                false,
            ),
            ("UPDATE user_feature_flags SET enabled = true WHERE user_id = $1", true),
            ("UPDATE user_feature_flags SET enabled = false WHERE user_id = $1", false),
            ("UPDATE user_feature_flags SET enabled = true WHERE user_id = $1", true),
            ("DELETE FROM user_feature_flags WHERE user_id = $1", false),
        ] {
            sqlx::query(sql).bind(account).execute(&pool).await.unwrap();
            let notification = timeout(Duration::from_secs(5), listener.recv()).await.unwrap().unwrap();
            assert!(notification.payload().starts_with("user_feature_flags:"));
            assert_eq!(FeatureFlags::new(&mut conn).has_feature(account, feature).await.unwrap(), expected);
            assert!(!FeatureFlags::new(&mut conn).has_feature(other, feature).await.unwrap());
        }
        sqlx::query("INSERT INTO user_feature_flags (user_id, feature_flag, enabled) VALUES ($1, 'FUTURE_FEATURE', true)")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!FeatureFlags::new(&mut conn).has_feature(account, feature).await.unwrap());
        sqlx::query("INSERT INTO user_feature_flags (user_id, feature_flag, enabled) VALUES ($1, 'ALLOW_NEGATIVE_BALANCE', true)")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET is_deleted = true WHERE id = $1")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!FeatureFlags::new(&mut conn).has_feature(account, feature).await.unwrap());
    }
}
