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
        (None, None, ApiKeyPurpose::Playground, None, 8, -2, None),
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
            ApiKeyPurpose::Playground => "playground",
            _ => "realtime",
        };
        let id: Uuid = sqlx::query_scalar("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window,user_id,serving_class,valid_from,valid_until) VALUES ($1,'test',$2,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
            .bind(model).bind(price).bind(purpose_name).bind(window).bind(scope).bind(class).bind(valid_from).bind(valid_until)
            .fetch_one(&pool).await.unwrap();
        tariffs.push(TariffInfo {
            id,
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
                (ApiKeyPurpose::Platform, "platform"),
            ] {
                for window in [None, Some("24h"), Some("1h")] {
                    for timestamp in [
                        now - chrono::Duration::hours(3),
                        now - chrono::Duration::hours(2),
                        now,
                        now + chrono::Duration::hours(1),
                        now + chrono::Duration::hours(2),
                    ] {
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
    // A realtime-only organization deal must not override the model batch tariff.
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
        Some(Decimal::ONE)
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
async fn customer_quotes_hide_classes_and_ownerless_estimates_use_general_prices(pool: PgPool) {
    use crate::db::handlers::{Tariffs, analytics::get_realtime_tariffs};
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('review-org','review@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('review-model','review-model',true,$1) RETURNING id").bind(account).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,'general',3,3,'realtime')").bind(model).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,$2,'standard','batch-deal',1,1,'batch','24h')").bind(model).bind(account).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tariffs = Tariffs::new(&mut conn);
    let quotes = tariffs.list_effective_for_account(&[model], account).await.unwrap();
    assert_eq!(quotes.len(), 1, "class-specific batch deals remain internal");
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
    assert_eq!(unknown_owner, None, "missing batch price must not borrow realtime");
    drop(conn);
    sqlx::query("UPDATE deployed_models SET deleted = TRUE WHERE id = $1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get_realtime_tariffs(&pool, account, &["review-model".to_string()]).await.unwrap()["review-model"],
        (Decimal::from(3), Decimal::from(3)),
        "historical usage retains a price after model deletion"
    );
}

#[sqlx::test]
async fn customer_price_sort_ignores_class_only_and_legacy_prices(pool: PgPool) {
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
    models.sort();
    assert_eq!(
        listed.iter().map(|m| m.id).collect::<Vec<_>>(),
        models,
        "none has a supported all-class price: all sort as unpriced, by id"
    );
}

#[sqlx::test]
async fn internal_purposes_never_resolve_customer_prices_or_quotes(pool: PgPool) {
    use crate::db::handlers::Tariffs;
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('internal-price-test','internal-price@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('internal-price','internal-price',true,$1) RETURNING id").bind(account).fetch_one(&pool).await.unwrap();
    for scope in [None, Some(account)] {
        for purpose in ["realtime", "continuation", "platform"] {
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,$2,$3,1,2,$3)").bind(model).bind(scope).bind(purpose).execute(&pool).await.unwrap();
        }
    }
    let mut conn = pool.acquire().await.unwrap();
    let mut repo = Tariffs::new(&mut conn);
    for (purpose, name) in [(ApiKeyPurpose::Continuation, "continuation"), (ApiKeyPurpose::Platform, "platform")] {
        assert_eq!(
            repo.get_pricing_at_timestamp(model, &purpose, Utc::now(), None).await.unwrap(),
            None
        );
        assert_eq!(
            repo.get_pricing_at_timestamp_with_fallback(model, Some(&purpose), &ApiKeyPurpose::Realtime, Utc::now(), None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            repo.get_effective_pricing_at_timestamp(model, Some(account), name, None, Some("interactive"), Utc::now())
                .await
                .unwrap(),
            None
        );
        let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,$2,$3)")
            .bind(model)
            .bind(account)
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!paid);
        let row = TariffInfo {
            id: Uuid::new_v4(),
            account: Some(account),
            serving_class: None,
            purpose: purpose.clone(),
            completion_window: None,
            input_price_per_token: Decimal::ONE,
            output_price_per_token: Decimal::ONE,
            effective_from: Utc::now() - chrono::Duration::hours(1),
            valid_until: None,
        };
        assert_eq!(
            find_best_tariff(&[row], Some(&purpose), None, Utc::now(), Some(account), None),
            (None, None)
        );
    }
    for rows in [
        repo.list_current_by_model(model).await.unwrap(),
        repo.list_current_by_model_all_scopes(model).await.unwrap(),
        repo.list_current_all_scopes_bulk(&[model]).await.unwrap(),
        repo.list_current_by_account(account).await.unwrap(),
        repo.list_effective_for_account(&[model], account).await.unwrap(),
    ] {
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.api_key_purpose == Some(ApiKeyPurpose::Realtime)));
    }
    assert_eq!(
        repo.list_all_by_model(model).await.unwrap().len(),
        3,
        "General historical rows remain readable"
    );
}

#[sqlx::test]
async fn general_price_sort_uses_current_windows_and_includes_free_prices(pool: PgPool) {
    use crate::api::models::deployments::ModelSortField;
    use crate::db::handlers::deployments::DeploymentFilter;
    use crate::db::handlers::{Deployments, Repository};
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username,email,auth_source) VALUES ('general-sort','general-sort@example.com','test') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut models = Vec::new();
    for alias in ["free", "finite", "open", "future-only", "internal-only"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
        )
        .bind(alias)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        models.push(id);
    }
    for (index, purpose, price, from, until) in [
        (0, "realtime", 0, -2, None),
        (1, "realtime", 1, -2, Some(1)),
        (1, "realtime", 9, 1, None),
        (2, "realtime", 2, -2, None),
        (2, "playground", 0, -4, Some(-1)),
        (3, "realtime", 1, 1, None),
        (4, "continuation", 0, -2, None),
        (4, "platform", 0, -2, None),
    ] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,api_key_purpose,input_price_per_token,output_price_per_token,valid_from,valid_until) VALUES ($1,'sort',$2,$3,$3,NOW()+$4*INTERVAL '1 hour',CASE WHEN $5::int IS NULL THEN NULL ELSE NOW()+$5*INTERVAL '1 hour' END)").bind(models[index]).bind(purpose).bind(Decimal::from(price)).bind(from as f64).bind(until).execute(&pool).await.unwrap();
    }
    let mut conn = pool.acquire().await.unwrap();
    let mut filter = DeploymentFilter::new(0, 100);
    filter.sort_field = Some(ModelSortField::PriceFrom);
    let listed = Deployments::new(&mut conn).list(&filter).await.unwrap();
    assert_eq!(
        listed[..3].iter().map(|m| m.alias.as_str()).collect::<Vec<_>>(),
        vec!["free", "finite", "open"]
    );
    assert_eq!(listed.len(), 5);
}

#[sqlx::test]
async fn realtime_admission_ignores_windowed_rows(pool: PgPool) {
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('window-test','window-test',true,'00000000-0000-0000-0000-000000000000') RETURNING id").fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,api_key_purpose,completion_window,input_price_per_token,output_price_per_token) VALUES ($1,'misconfigured','realtime','24h',1,2)").bind(model).execute(&pool).await.unwrap();
    let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,NULL,'realtime')")
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!paid, "admission must not invent a realtime completion window");
}

/// Exhaust every combination of the six eligible playground candidates. The
/// ordered fixture is the business contract, independent of either resolver's
/// implementation. Deliberately make the most specific realtime deal free.
#[sqlx::test]
async fn playground_exhausts_each_customer_scope_before_general_prices(pool: PgPool) {
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('playground-deal','playground-deal@example.com','test','organization') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('playground-deal','playground-deal',true,$1) RETURNING id")
        .bind(account).fetch_one(&pool).await.unwrap();
    let now = Utc::now();
    let candidates = [
        (Some(account), Some("interactive"), ApiKeyPurpose::Playground, 11),
        (Some(account), Some("interactive"), ApiKeyPurpose::Realtime, 0),
        (Some(account), None, ApiKeyPurpose::Playground, 13),
        (Some(account), None, ApiKeyPurpose::Realtime, 14),
        (None, None, ApiKeyPurpose::Playground, 15),
        (None, None, ApiKeyPurpose::Realtime, 16),
    ];
    for mask in 0..64 {
        sqlx::query("DELETE FROM model_tariffs WHERE deployed_model_id=$1")
            .bind(model)
            .execute(&pool)
            .await
            .unwrap();
        let mut rows = Vec::new();
        for (index, (owner, class, purpose, price)) in candidates.iter().enumerate() {
            if mask & (1 << index) == 0 {
                continue;
            }
            let name = if *purpose == ApiKeyPurpose::Playground {
                "playground"
            } else {
                "realtime"
            };
            let price = Decimal::from(*price);
            let id: Uuid = sqlx::query_scalar("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,api_key_purpose,input_price_per_token,output_price_per_token,valid_from) VALUES ($1,$2,$3,'precedence',$4,$5,$5,$6) RETURNING id")
                .bind(model).bind(owner).bind(class).bind(name).bind(price).bind(now-chrono::Duration::hours(1)).fetch_one(&pool).await.unwrap();
            rows.push(TariffInfo {
                id,
                account: *owner,
                serving_class: class.map(str::to_owned),
                purpose: purpose.clone(),
                completion_window: None,
                input_price_per_token: price,
                output_price_per_token: price,
                effective_from: now - chrono::Duration::hours(1),
                valid_until: None,
            });
        }
        // Reverse input order: ledger/query iteration order must not select the winner.
        rows.reverse();
        for (purpose, purpose_name) in [(ApiKeyPurpose::Playground, "playground"), (ApiKeyPurpose::Realtime, "realtime")] {
            for (owner, class) in [
                (Some(account), Some("interactive")),
                (Some(account), Some("throughput")),
                (None, Some("interactive")),
                (Some(Uuid::new_v4()), Some("interactive")),
            ] {
                let expected = candidates
                    .iter()
                    .enumerate()
                    .find(|(index, (scope, c, p, _))| {
                        mask & (1 << index) != 0
                            && (scope.is_none() || *scope == owner)
                            && (c.is_none() || *c == class)
                            && (purpose == ApiKeyPurpose::Playground || *p == ApiKeyPurpose::Realtime)
                    })
                    .map(|(_, (_, _, _, price))| (Decimal::from(*price), Decimal::from(*price)));
                let sql: Option<(Decimal, Decimal)> =
                    sqlx::query_as("SELECT input_price_per_token,output_price_per_token FROM effective_model_tariff($1,$2,$3,NULL,$4,$5)")
                        .bind(model)
                        .bind(owner)
                        .bind(purpose_name)
                        .bind(class)
                        .bind(now)
                        .fetch_optional(&pool)
                        .await
                        .unwrap();
                let rust = find_best_tariff(&rows, Some(&purpose), None, now, owner, class);
                assert_eq!(
                    sql, expected,
                    "SQL mask={mask} purpose={purpose_name} owner={owner:?} class={class:?}"
                );
                assert_eq!(
                    rust,
                    expected.map(|(i, o)| (Some(i), Some(o))).unwrap_or((None, None)),
                    "Rust mask={mask} purpose={purpose_name} owner={owner:?} class={class:?}"
                );
            }
        }
    }
}

#[sqlx::test]
async fn equal_timestamp_tariffs_use_the_same_stable_id_in_sql_and_billing(pool: PgPool) {
    let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('tie-test','tie-test',true,'00000000-0000-0000-0000-000000000000') RETURNING id").fetch_one(&pool).await.unwrap();
    let now = Utc::now();
    let mut rows = Vec::new();
    // Deliberately overlapping, finite historical versions with identical start
    // times are legal in the ledger. Query/iteration order must not affect cost.
    for number in [2, 1] {
        let id = Uuid::from_u128(number);
        let price = Decimal::from(number as u64);
        sqlx::query("INSERT INTO model_tariffs (id,deployed_model_id,name,api_key_purpose,input_price_per_token,output_price_per_token,valid_from,valid_until) VALUES ($1,$2,'tie','realtime',$3,$3,$4,$5)")
            .bind(id).bind(model).bind(price).bind(now-chrono::Duration::hours(1)).bind(now+chrono::Duration::hours(1)).execute(&pool).await.unwrap();
        rows.push(TariffInfo {
            id,
            account: None,
            serving_class: None,
            purpose: ApiKeyPurpose::Realtime,
            completion_window: None,
            input_price_per_token: price,
            output_price_per_token: price,
            effective_from: now - chrono::Duration::hours(1),
            valid_until: Some(now + chrono::Duration::hours(1)),
        });
    }
    let selected: Uuid = sqlx::query_scalar("SELECT id FROM effective_model_tariff($1,NULL,'playground',NULL,NULL,$2)")
        .bind(model)
        .bind(now)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(selected, Uuid::from_u128(1));
    for _ in 0..2 {
        assert_eq!(
            find_best_tariff(&rows, Some(&ApiKeyPurpose::Playground), None, now, None, None),
            (Some(Decimal::ONE), Some(Decimal::ONE))
        );
        rows.reverse();
    }
}

#[sqlx::test]
async fn customer_display_sort_and_usage_have_explicitly_different_class_rules(pool: PgPool) {
    use crate::api::models::deployments::ModelSortField;
    use crate::db::handlers::{Deployments, Repository, Tariffs, analytics::get_realtime_tariffs, deployments::DeploymentFilter};
    let account: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('display-org','display@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
    let mut models = Vec::new();
    for alias in ["display-deal", "display-comparator"] {
        let model: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
        )
        .bind(alias)
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,'general',3,3,'realtime')").bind(model).execute(&pool).await.unwrap();
        models.push(model);
    }
    let model = models[0];
    for (class, price) in [(None, 2), (Some("standard"), 9), (Some("interactive"), 0)] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,$2,$3,'deal', $4,$4,'realtime')").bind(model).bind(account).bind(class).bind(Decimal::from(price)).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,'batch',1,1,'batch','24h')").bind(model).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,$2,'standard','free-batch',0,0,'batch','24h')").bind(model).bind(account).execute(&pool).await.unwrap();
    // Future and expired rows must not replace the currently applicable deal.
    sqlx::query("UPDATE model_tariffs SET valid_until=NOW()+INTERVAL '1 hour' WHERE user_id=$1 AND serving_class IS NULL")
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose,valid_from) VALUES ($1,$2,'future',99,99,'realtime',NOW()+INTERVAL '1 hour')").bind(model).bind(account).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let quotes = Tariffs::new(&mut conn).list_effective_for_account(&[model], account).await.unwrap();
    assert_eq!(quotes.len(), 2);
    assert!(quotes.iter().all(|t| t.serving_class.is_none()));
    assert_eq!(
        quotes
            .iter()
            .find(|t| t.api_key_purpose == Some(ApiKeyPurpose::Realtime))
            .unwrap()
            .input_price_per_token,
        Decimal::from(2)
    );
    assert_eq!(
        quotes
            .iter()
            .find(|t| t.api_key_purpose == Some(ApiKeyPurpose::Batch))
            .unwrap()
            .input_price_per_token,
        Decimal::ONE
    );
    let stranger = Tariffs::new(&mut conn)
        .list_effective_for_account(&[model], Uuid::new_v4())
        .await
        .unwrap();
    assert!(stranger.iter().all(|t| t.user_id.is_none()));
    for (class, expected) in [("standard", 9), ("interactive", 0)] {
        let price = Tariffs::new(&mut conn)
            .get_effective_pricing_at_timestamp(model, Some(account), "realtime", None, Some(class), Utc::now())
            .await
            .unwrap();
        assert_eq!(price.unwrap().0, Decimal::from(expected), "billing still uses class {class}");
    }
    let mut filter = DeploymentFilter::new(0, 100);
    filter.sort_field = Some(ModelSortField::PriceFrom);
    filter.pricing_account = Some(account);
    assert_eq!(Deployments::new(&mut conn).list(&filter).await.unwrap()[0].id, model);
    drop(conn);
    for (expired_class, expected) in [(None, 2), (Some("all"), 9), (Some("standard"), 3)] {
        if let Some(class) = expired_class {
            sqlx::query("UPDATE model_tariffs SET valid_until=NOW() WHERE user_id=$1 AND api_key_purpose='realtime' AND valid_from<=NOW() AND serving_class IS NOT DISTINCT FROM $2").bind(account).bind(if class=="all" {None} else {Some(class)}).execute(&pool).await.unwrap();
        }
        assert_eq!(
            get_realtime_tariffs(&pool, account, &["display-deal".to_string()]).await.unwrap()["display-deal"].0,
            Decimal::from(expected)
        );
    }
    // A zero interactive deal cannot make an otherwise expensive model sort first.
    sqlx::query("UPDATE model_tariffs SET input_price_per_token=5,output_price_per_token=5 WHERE deployed_model_id=$1 AND user_id IS NULL")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(Deployments::new(&mut conn).list(&filter).await.unwrap()[0].id, models[1]);
}

#[sqlx::test]
async fn legacy_paid_admission_respects_explicit_zero_batch_windows(pool: PgPool) {
    let account: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username,email,auth_source) VALUES ('free-batch','free-batch@example.com','test') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let model: Uuid = sqlx::query_scalar(
        "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('free-batch','free-batch',true,$1) RETURNING id",
    )
    .bind(account)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token) VALUES ($1,'legacy',1,1)")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    for window in ["1h", "24h"] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,'batch',1,1,'batch',$2)").bind(model).bind(window).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,$2,'free-batch',0,0,'batch',$3)").bind(model).bind(account).bind(window).execute(&pool).await.unwrap();
    }
    let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,$2,'batch')")
        .bind(model)
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!paid, "legacy NULL-purpose rows must not override explicit free batch windows");
    sqlx::query("UPDATE model_tariffs SET input_price_per_token=1 WHERE user_id=$1 AND completion_window='1h'")
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
    let paid: bool = sqlx::query_scalar("SELECT model_has_effective_paid_tariff($1,$2,'batch')")
        .bind(model)
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(paid, "one paid batch window still requires credit");
}

#[sqlx::test]
async fn historical_deals_prevent_hard_account_deletion(pool: PgPool) {
    let model: Uuid=sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('retention','retention',true,'00000000-0000-0000-0000-000000000000') RETURNING id").fetch_one(&pool).await.unwrap();
    for table in ["model_tariffs", "model_cache_tariffs"] {
        let account: Uuid = sqlx::query_scalar(
            "INSERT INTO users (username,email,auth_source,user_type) VALUES ($1,$1,'test','organization') RETURNING id",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        if table == "model_tariffs" {
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose,valid_from,valid_until) VALUES ($1,$2,'historical',1,1,'realtime',NOW()-INTERVAL '2 days',NOW()-INTERVAL '1 day')").bind(model).bind(account).execute(&pool).await.unwrap();
        } else {
            sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,user_id,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,read_multiplier,min_prefix_tokens,valid_from,valid_until) VALUES ($1,$2,1,1,1,1,1,NOW()-INTERVAL '2 days',NOW()-INTERVAL '1 day')").bind(model).bind(account).execute(&pool).await.unwrap();
        }
        let error = sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().constraint(),
            Some(format!("{table}_user_id_fkey").as_str())
        );
        sqlx::query("UPDATE users SET is_deleted=true WHERE id=$1")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE user_id=$1"))
            .bind(account)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "soft deletion retains historical prices");
    }
}
