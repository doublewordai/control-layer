use super::*;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn sql_and_billing_agree_on_class_scope_window_and_history(pool: PgPool) {
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('class-deal','class@example.com','test','organization') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let model: Uuid = sqlx::query_scalar(
        "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('class-model','class-model',true,$1) RETURNING id",
    )
    .bind(account)
    .fetch_one(&pool)
    .await
    .unwrap();
    let now = Utc::now();
    let mut tariffs = Vec::new();
    for (scope, class, purpose, window, price, from, until) in [
        (None, None, ApiKeyPurpose::Realtime, None, 3, -2, None),
        (None, None, ApiKeyPurpose::Batch, Some("24h"), 1, -2, None),
        (Some(account), None, ApiKeyPurpose::Realtime, None, 2, -2, None),
        (Some(account), Some("interactive"), ApiKeyPurpose::Realtime, None, 0, -2, Some(1)),
        (Some(account), Some("interactive"), ApiKeyPurpose::Realtime, None, 9, 1, None),
        (Some(account), Some("standard"), ApiKeyPurpose::Batch, Some("1h"), 4, -2, None),
    ] {
        let valid_from = now + chrono::Duration::hours(from);
        let valid_until = until.map(|hours| now + chrono::Duration::hours(hours));
        let price = Decimal::from(price);
        let purpose_name = match purpose {
            ApiKeyPurpose::Batch => "batch",
            _ => "realtime",
        };
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window,user_id,serving_class,valid_from,valid_until) VALUES ($1,'test',$2,$2,$3,$4,$5,$6,$7,$8)")
            .bind(model).bind(price).bind(purpose_name).bind(window).bind(scope).bind(class).bind(valid_from).bind(valid_until)
            .execute(&pool).await.unwrap();
        tariffs.push(TariffInfo {
            account: scope,
            serving_class: class.map(str::to_owned),
            purpose,
            completion_window: window.map(str::to_owned),
            input_price_per_token: price,
            output_price_per_token: price,
            effective_from: valid_from,
            valid_until,
        });
    }
    for scope in [None, Some(account), Some(Uuid::new_v4())] {
        for class in [None, Some("standard"), Some("interactive"), Some("throughput"), Some("custom")] {
            for (purpose, name) in [
                (ApiKeyPurpose::Realtime, "realtime"),
                (ApiKeyPurpose::Batch, "batch"),
                (ApiKeyPurpose::Playground, "playground"),
                (ApiKeyPurpose::Continuation, "continuation"),
            ] {
                for window in [None, Some("24h"), Some("1h")] {
                    for timestamp in [now - chrono::Duration::hours(3), now, now + chrono::Duration::hours(2)] {
                        let sql: Option<(Decimal, Decimal)> = sqlx::query_as(
                            "SELECT input_price_per_token,output_price_per_token FROM effective_model_tariff($1,$2,$3,$4,$5,$6)",
                        )
                        .bind(model)
                        .bind(scope)
                        .bind(name)
                        .bind(window)
                        .bind(class)
                        .bind(timestamp)
                        .fetch_optional(&pool)
                        .await
                        .unwrap();
                        let rust = find_best_tariff(&tariffs, Some(&purpose), window, timestamp, scope, class);
                        assert_eq!(
                            rust,
                            sql.map(|(i, o)| (Some(i), Some(o))).unwrap_or((None, None)),
                            "{scope:?} {class:?} {name} {window:?} {timestamp}"
                        );
                    }
                }
            }
        }
    }
    // A class-agnostic org realtime deal outranks a general batch-specific price.
    assert_eq!(
        find_best_tariff(
            &tariffs,
            Some(&ApiKeyPurpose::Batch),
            Some("24h"),
            now,
            Some(account),
            Some("standard")
        )
        .0,
        Some(Decimal::from(2))
    );
    assert_eq!(
        find_best_tariff(
            &tariffs,
            Some(&ApiKeyPurpose::Realtime),
            None,
            now,
            Some(account),
            Some("interactive")
        )
        .0,
        Some(Decimal::ZERO)
    );
    // The model-level gate sees the actual all-class deal, including explicit zero.
    sqlx::query("UPDATE model_tariffs SET input_price_per_token=0,output_price_per_token=0 WHERE user_id=$1")
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
    let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,$2,'realtime')")
        .bind(model)
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!paid);
    let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,NULL,'realtime')")
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(paid);
}

#[test]
fn cache_class_deals_require_a_general_enablement_row_and_respect_history() {
    let now = Utc::now();
    let account = Uuid::new_v4();
    let row = |scope, class: Option<&str>, read| CacheTariffRow {
        account: scope,
        serving_class: class.map(str::to_owned),
        valid_from: now - chrono::Duration::hours(1),
        valid_until: None,
        write_multiplier_5m: Decimal::ONE,
        write_multiplier_1h: Decimal::ONE,
        write_multiplier_24h: Decimal::ONE,
        read_multiplier: read,
    };
    let own = row(Some(account), None, Decimal::new(5, 1));
    let interactive = row(Some(account), Some("interactive"), Decimal::ZERO);
    assert!(resolve_cache_multipliers(&[own.clone(), interactive.clone()], now, Some(account), Some("interactive")).is_none());
    let rows = vec![row(None, None, Decimal::ONE), own, interactive];
    assert_eq!(
        resolve_cache_multipliers(&rows, now, Some(account), Some("interactive"))
            .unwrap()
            .read,
        Decimal::ZERO
    );
    assert_eq!(
        resolve_cache_multipliers(&rows, now, Some(account), Some("throughput"))
            .unwrap()
            .read,
        Decimal::new(5, 1)
    );
    assert_eq!(
        resolve_cache_multipliers(&rows, now, Some(Uuid::new_v4()), Some("interactive"))
            .unwrap()
            .read,
        Decimal::ONE
    );
    assert!(resolve_cache_multipliers(&rows, now - chrono::Duration::hours(2), Some(account), Some("interactive")).is_none());
}

#[sqlx::test]
async fn quotes_keep_matched_classes_and_ownerless_estimates_use_general_prices(pool: PgPool) {
    use crate::db::handlers::{Tariffs, analytics::get_realtime_tariffs};
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('review-org','review@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('review-model','review-model',true,$1) RETURNING id").bind(account).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,'general',3,3,'realtime')").bind(model).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,$2,'standard','batch-deal',1,1,'batch','24h')").bind(model).bind(account).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tariffs = Tariffs::new(&mut conn);
    let quotes = tariffs.list_effective_for_account(&[model], account).await.unwrap();
    assert_eq!(quotes.len(), 2, "a batch class must not duplicate the general realtime quote");
    let general = quotes.iter().find(|q| q.name == "general").unwrap();
    assert_eq!(general.serving_class, None, "fallback must retain the matched price's class");
    let own = tariffs
        .get_effective_pricing_at_timestamp(model, Some(account), "batch", Some("24h"), Some("standard"), Utc::now())
        .await
        .unwrap();
    let unknown_owner = tariffs
        .get_effective_pricing_at_timestamp(model, None, "batch", Some("24h"), Some("standard"), Utc::now())
        .await
        .unwrap();
    assert_eq!(own, Some((Decimal::ONE, Decimal::ONE)));
    assert_eq!(unknown_owner, Some((Decimal::from(3), Decimal::from(3))));
    drop(conn);
    sqlx::query("UPDATE deployed_models SET deleted = TRUE WHERE id = $1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get_realtime_tariffs(&pool, account).await.unwrap()["review-model"],
        (Decimal::from(3), Decimal::from(3)),
        "historical usage retains a price after model deletion"
    );
}

#[sqlx::test]
async fn customer_price_sort_keeps_legacy_and_class_only_prices(pool: PgPool) {
    use crate::api::models::deployments::ModelSortField;
    use crate::db::handlers::deployments::DeploymentFilter;
    use crate::db::handlers::{Deployments, Repository};
    let account: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username,email,auth_source) VALUES ('sort-review','sort-review@example.com','test') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut models = Vec::new();
    for alias in ["legacy", "class-only", "unpriced"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
        )
        .bind(alias)
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
        models.push(id);
    }
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token) VALUES ($1,'legacy',1,1)")
        .bind(models[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,$2,'interactive','class',2,2,'realtime')").bind(models[1]).bind(account).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut filter = DeploymentFilter::new(0, 100);
    filter.sort_field = Some(ModelSortField::PriceFrom);
    filter.pricing_account = Some(account);
    let listed = Deployments::new(&mut conn).list(&filter).await.unwrap();
    assert_eq!(
        listed.iter().map(|m| m.alias.as_str()).collect::<Vec<_>>(),
        vec!["legacy", "class-only", "unpriced"]
    );
}
