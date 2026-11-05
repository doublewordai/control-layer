//! The migrations that stop storing API key secrets: the retained-payload
//! scrub removes every stored key and is safe to re-run, and the down
//! migrations restore the columns and views the previous release reads.

use std::borrow::Cow;

use fusillade_arsenal::MIGRATOR;
use sqlx::PgPool;
use uuid::Uuid;

/// The last migration of the release that still has the key columns.
const BEFORE_DROP: i64 = 20261105105806;
const DROP_BATCHES_API_KEY: i64 = 20261105143830;
const SCRUB_RETAINED_PAYLOADS: i64 = 20261105143840;

fn migrator_where(predicate: impl Fn(i64) -> bool) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| predicate(migration.version))
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

/// Request objects spread over five daily partitions (2000 with a stored key,
/// 500 without), plus group objects. Every update the scrub makes is logged
/// by a trigger, which can also fail the run partway as an interrupted Job.
async fn seed_retained(pool: &PgPool) {
    migrator_where(|version| version <= DROP_BATCHES_API_KEY)
        .run(pool)
        .await
        .expect("migrations before the scrub must apply");
    sqlx::query(
        "SELECT ensure_retained_response_partition((now() AT TIME ZONE 'UTC')::date + d, NULL)
         FROM generate_series(2, 6) AS d",
    )
    .execute(pool)
    .await
    .expect("retained response partitions must be available");
    sqlx::query(
        r#"
        INSERT INTO retained_response_objects
            (delete_on, group_id, object_kind, object_id, request_id, schema_version, payload)
        SELECT (now() AT TIME ZONE 'UTC')::date + 2 + n % 5, id, kind, id,
               CASE WHEN kind = 'request' THEN id END, 1,
               CASE
                   WHEN kind = 'group' THEN jsonb_build_object('group_id', id, 'request_ids', jsonb_build_array(id))
                   WHEN n <= 2000 THEN jsonb_build_object('request', jsonb_build_object('n', n),
                                                          'template', jsonb_build_object('body', 'b', 'api_key', 'sk-stored-' || n))
                   ELSE jsonb_build_object('request', jsonb_build_object('n', n), 'template', jsonb_build_object('body', 'b'))
               END
        FROM (
            SELECT n, gen_random_uuid() AS id, CASE WHEN n > 2500 THEN 'group' ELSE 'request' END AS kind
            FROM generate_series(1, 2600) AS n
        ) seed
        "#,
    )
    .execute(pool)
    .await
    .expect("retained objects must insert");
    sqlx::raw_sql(
        r#"
        CREATE TABLE scrub_log (object_id UUID NOT NULL);
        CREATE TABLE scrub_fail_after (updates BIGINT);
        INSERT INTO scrub_fail_after VALUES (NULL);
        CREATE FUNCTION log_scrub() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF (SELECT updates FROM scrub_fail_after) <= (SELECT COUNT(*) FROM scrub_log) THEN
                RAISE EXCEPTION 'interrupted scrub';
            END IF;
            INSERT INTO scrub_log VALUES (NEW.object_id);
            RETURN NEW;
        END
        $$;
        CREATE TRIGGER log_scrub BEFORE UPDATE ON retained_response_objects
            FOR EACH ROW EXECUTE FUNCTION log_scrub();
        "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn run_scrub(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    migrator_where(|version| version <= SCRUB_RETAINED_PAYLOADS)
        .run(pool)
        .await
}

/// Every keyed request object is rewritten exactly once and loses its key;
/// keyless and group objects are never touched.
async fn assert_scrubbed_once(pool: &PgPool) {
    let keyed_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_objects WHERE payload -> 'template' ? 'api_key'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(keyed_rows, 0, "no payload keeps a key");
    let (logged, distinct, wrong): (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*), COUNT(DISTINCT log.object_id),
               COUNT(*) FILTER (WHERE (object.payload -> 'request' ->> 'n')::int > 2000 OR object.object_kind <> 'request')
        FROM scrub_log log
        JOIN retained_response_objects object ON object.object_id = log.object_id
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((logged, distinct, wrong), (2000, 2000, 0));
}

#[sqlx::test(migrations = false)]
async fn the_scrub_removes_every_retained_key_once_across_partitions(pool: PgPool) {
    seed_retained(&pool).await;
    run_scrub(&pool).await.expect("the scrub must apply");
    assert_scrubbed_once(&pool).await;

    // A re-run of the statement changes nothing.
    let scrub = MIGRATOR
        .iter()
        .find(|migration| {
            migration.version == SCRUB_RETAINED_PAYLOADS
                && !migration.migration_type.is_down_migration()
        })
        .expect("the scrub migration");
    sqlx::raw_sql(&scrub.sql)
        .execute(&pool)
        .await
        .expect("the scrub re-runs");
    assert_scrubbed_once(&pool).await;
}

#[sqlx::test(migrations = false)]
async fn an_interrupted_scrub_resumes_without_rewriting_finished_rows(pool: PgPool) {
    seed_retained(&pool).await;
    // The run fails in a later batch, after earlier batches have committed.
    sqlx::query("UPDATE scrub_fail_after SET updates = 1200")
        .execute(&pool)
        .await
        .unwrap();
    run_scrub(&pool)
        .await
        .expect_err("the interrupted run fails");
    let (done, remaining): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM scrub_log),
                (SELECT COUNT(*) FROM retained_response_objects WHERE payload -> 'template' ? 'api_key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(done > 0 && done < 2000, "committed batches persist: {done}");
    assert_eq!(done + remaining, 2000);

    sqlx::query("UPDATE scrub_fail_after SET updates = NULL")
        .execute(&pool)
        .await
        .unwrap();
    run_scrub(&pool)
        .await
        .expect("the retried scrub must apply");
    assert_scrubbed_once(&pool).await;
}

async fn column(pool: &PgPool, table: &str) -> Option<(i32, String, Option<String>)> {
    sqlx::query_as(
        "SELECT ordinal_position, is_nullable, column_default FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = 'api_key'",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn the_down_migrations_restore_the_columns_and_views(pool: PgPool) {
    MIGRATOR
        .run(&pool)
        .await
        .expect("every migration must apply");
    for table in [
        "request_templates_g2",
        "batches",
        "active_request_templates",
        "request_templates_all",
        "request_templates",
    ] {
        assert_eq!(column(&pool, table).await, None, "{table} has no api_key");
    }

    MIGRATOR
        .undo(&pool, BEFORE_DROP)
        .await
        .expect("the down migrations must apply");

    let (_, nullable, default) = column(&pool, "request_templates_g2")
        .await
        .expect("g2 api_key");
    assert_eq!(nullable, "NO");
    assert_eq!(default.as_deref(), Some("''::text"));
    let (_, nullable, _) = column(&pool, "batches").await.expect("batches api_key");
    assert_eq!(nullable, "YES");
    // In the position the previous release's views had it: after model.
    for view in [
        "active_request_templates",
        "request_templates_all",
        "request_templates",
    ] {
        let (position, _, _) = column(&pool, view)
            .await
            .unwrap_or_else(|| panic!("{view} api_key"));
        assert_eq!(position, 8, "{view}");
    }

    // The restored schema accepts the previous release's writes, which omit
    // the key from templates.
    let week: chrono::NaiveDate =
        sqlx::query_scalar("SELECT date_trunc('week', now() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates_g2 (created_on, endpoint, method, path, model)
         VALUES ($1, 'e', 'POST', '/x', 'm') RETURNING id",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .expect("a template without a key must insert");
    let stored: String =
        sqlx::query_scalar("SELECT api_key FROM request_templates_g2 WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "");
}
