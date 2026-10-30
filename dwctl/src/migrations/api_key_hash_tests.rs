//! The API key hash backfill and NOT NULL check (DW-1005), via SQLx.
use uuid::Uuid;

use super::*;
use crate::crypto::{api_key_prefix, hash_api_key};
use crate::db::handlers::api_keys::ApiKeys;

/// The last migration of the release that added `secret_hash` without a backfill.
const BEFORE_BACKFILL: i64 = 20261029120030;

/// Rows written before the hash column was populated: the seeded system key,
/// several backfill batches' worth of generated keys, and a non-ASCII secret
/// to pin `convert_to(secret, 'UTF8')` to Rust's `str::as_bytes`.
#[sqlx::test(migrations = false)]
async fn backfill_hashes_existing_keys_like_hash_api_key(pool: PgPool) {
    let target = Target::main();
    target.run_to(BEFORE_BACKFILL, &pool).await.unwrap();

    let user: Uuid = sqlx::query_scalar("INSERT INTO users (username,email) VALUES ('hash-test','hash-test@example.invalid') RETURNING id")
        .fetch_one(&pool)
        .await
        .unwrap();
    let unicode_secret = "sk-ünïcödé-ключ-🔑";
    sqlx::query("INSERT INTO api_keys (name, secret, user_id, created_by, purpose) VALUES ('unicode', $1, $2, $2, 'realtime')")
        .bind(unicode_secret)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, secret, user_id, created_by, purpose)
         SELECT 'bulk-' || n, 'sk-bulk-' || n || '-' || md5(n::text), $1, $1, 'realtime'
         FROM generate_series(1, 12001) AS n",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    // A row the dual-writing release already hashed is left alone.
    sqlx::query(
        "INSERT INTO api_keys (name, secret, secret_hash, secret_prefix, user_id, created_by, purpose)
         VALUES ('already-hashed', 'sk-already-hashed', $1, 'keep-me!', $2, $2, 'realtime')",
    )
    .bind(&hash_api_key("sk-already-hashed")[..])
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();

    apply(&target, &pool).await.unwrap();

    let rows: Vec<(Uuid, String, Option<Vec<u8>>, Option<String>)> =
        sqlx::query_as("SELECT id, secret, secret_hash, secret_prefix FROM api_keys")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 12004, "system key + unicode + bulk + already hashed");
    assert!(rows.iter().any(|(id, ..)| id.is_nil()), "the seeded system key is included");
    for (id, secret, hash, prefix) in &rows {
        assert_eq!(
            hash.as_deref(),
            Some(&hash_api_key(secret)[..]),
            "{id}: SQL hash differs from hash_api_key"
        );
        let expected_prefix = if secret == "sk-already-hashed" {
            "keep-me!".to_string()
        } else {
            api_key_prefix(secret)
        };
        assert_eq!(prefix.as_deref(), Some(expected_prefix.as_str()), "{id}");
    }

    // A key that only had plaintext before the backfill now authenticates by hash.
    let mut conn = pool.acquire().await.unwrap();
    let mut keys = ApiKeys::new(&mut conn);
    assert_eq!(keys.get_user_id_by_secret(unicode_secret).await.unwrap(), Some(user));
    assert_eq!(
        keys.get_user_id_by_secret("sk-bulk-7-8f14e45fceea167a5a36dedd4bea2543")
            .await
            .unwrap(),
        Some(user)
    );
    let info = keys.get_user_info_by_secret(unicode_secret).await.unwrap().unwrap();
    assert_eq!(info.0, user);
    assert_eq!(keys.get_user_id_by_secret("sk-not-a-key").await.unwrap(), None);
    drop(conn);

    // The check is validated, and a write without a hash is refused.
    let validated: bool = sqlx::query_scalar("SELECT convalidated FROM pg_constraint WHERE conname = 'api_keys_secret_hash_not_null'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(validated);
    let error = sqlx::query(
        "INSERT INTO api_keys (name, secret, user_id, created_by, purpose) VALUES ('no-hash', 'sk-no-hash', $1, $1, 'realtime')",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23514"));
}

/// Rotating a key's hash reloads the onwards cache; changing only the plaintext
/// column (which nothing reads for lookups any more) does not.
#[dwctl_test_macros::test]
async fn api_keys_notify_fires_on_secret_hash_not_secret(pool: PgPool) {
    let key: Uuid = sqlx::query_scalar(
        "INSERT INTO api_keys (name, secret, secret_hash, secret_prefix, user_id, created_by, purpose)
         VALUES ('notify', 'sk-notify', sha256('sk-notify'::bytea), 'sk-notif', $1, $1, 'realtime') RETURNING id",
    )
    .bind(Uuid::nil())
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await.unwrap();
    listener.listen("auth_config_changed").await.unwrap();
    let mut next_payload = async || {
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
            .await
            .expect("notification")
            .unwrap()
            .payload()
            .to_string()
    };

    // Notifications arrive in commit order: if the plaintext-only update
    // notified, its payload would arrive before the marker.
    sqlx::query("UPDATE api_keys SET secret = 'sk-plaintext-only' WHERE id = $1")
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT pg_notify('auth_config_changed', 'marker')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET secret_hash = sha256('sk-rotated'::bytea) WHERE id = $1")
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(next_payload().await, "marker", "a plaintext-only update must not notify");
    assert!(next_payload().await.starts_with("api_keys:"), "a hash change notifies");
}
