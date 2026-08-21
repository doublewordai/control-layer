use fusillade_arsenal::manager::RetainedResponseRetirementOutcome;
use fusillade_arsenal::{
    DaemonStorage, PostgresRequestManager, PostgresStorageConfig, TestDbPools,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn partition_name(delete_on: chrono::NaiveDate) -> String {
    format!("retained_response_objects_d{}", delete_on.format("%Y%m%d"))
}

async fn ensure_partition(pool: &PgPool, delete_on: chrono::NaiveDate) {
    sqlx::query("SELECT ensure_retained_response_partition($1, NULL)")
        .bind(delete_on)
        .execute(pool)
        .await
        .expect("daily partition must be created");
}

async fn maintenance_pool(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("dedicated maintenance pool must connect")
}

async fn retirement_manager(pool: &PgPool) -> PostgresRequestManager<TestDbPools> {
    let maintenance_pool = maintenance_pool(pool).await;
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone())
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600))
    .with_partition_maintenance_pool(maintenance_pool)
    .expect("a max-one/min-zero pool must be accepted")
    .attest_partition_maintenance_pool()
    .await
    .expect("the test pool must target the exact writable schema")
}

async fn install_pending_journal(pool: &PgPool, delete_on: chrono::NaiveDate) {
    let mut tx = pool.begin().await.expect("journal transaction must begin");
    sqlx::query(
        r#"
        INSERT INTO retention_partition_retirements (
            parent_table, partition_table, partition_oid,
            partition_schema, partition_schema_oid, parent_oid,
            lower_bound, upper_bound
        )
        SELECT 'retained_response_objects', bucket.partition_table, child.oid,
               namespace.nspname, namespace.oid, parent.oid, $1, $1 + 1
        FROM retained_response_buckets bucket
        JOIN pg_class child ON child.oid = bucket.partition_oid
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        JOIN pg_class parent ON parent.oid = 'retained_response_objects'::regclass
        WHERE bucket.delete_on = $1
        "#,
    )
    .bind(delete_on)
    .execute(&mut *tx)
    .await
    .expect("pending journal must be inserted");
    sqlx::query(
        "UPDATE retained_response_buckets \
         SET state = 'retiring', state_changed_at = statement_timestamp() \
         WHERE delete_on = $1 AND state = 'active'",
    )
    .bind(delete_on)
    .execute(&mut *tx)
    .await
    .expect("bucket fence must be installed");
    tx.commit().await.expect("journal transaction must commit");
}

#[sqlx::test]
async fn retirement_requires_an_explicit_single_session_pool(pool: PgPool) {
    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool)
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    );
    let error = manager
        .retire_expired_response_partition(true)
        .await
        .expect_err("partition DDL must fail closed without a session pool");
    assert_eq!(
        error.to_string(),
        "Retained response partition maintenance pool is not configured"
    );
}

#[sqlx::test]
async fn partition_maintenance_pool_rejects_any_shape_except_max_one_min_zero(pool: PgPool) {
    let wrong_max = PgPoolOptions::new()
        .max_connections(2)
        .min_connections(0)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("wrong-max pool must connect for validation");
    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone())
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    );
    assert!(manager.with_partition_maintenance_pool(wrong_max).is_err());

    let wrong_min = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("wrong-min pool must connect for validation");
    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool)
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    );
    assert!(manager.with_partition_maintenance_pool(wrong_min).is_err());
}

#[sqlx::test]
async fn maintenance_pool_attestation_rejects_a_different_schema(pool: PgPool) {
    sqlx::query("CREATE SCHEMA wrong_retirement_target")
        .execute(&pool)
        .await
        .unwrap();
    let options = pool
        .connect_options()
        .as_ref()
        .clone()
        .options([("search_path", "wrong_retirement_target")]);
    let wrong_schema = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .connect_with(options)
        .await
        .unwrap();
    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_partition_maintenance_pool(wrong_schema)
    .unwrap();
    assert!(!manager.supports_retained_response_partition_retirement());
    let error = match manager.attest_partition_maintenance_pool().await {
        Ok(_) => panic!("a different schema must never gain retirement capability"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "Validation error: partition maintenance pool must target the exact writable retained response schema"
    );
}

#[sqlx::test]
async fn partition_lock_identity_is_independent_of_date_style(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL DateStyle = 'German, DMY'")
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(\
             'retained_response_objects.partition:' || current_schema() || ':'\
                 || to_char($1::date, 'YYYYMMDD'), 0))",
    )
    .bind(today)
    .execute(&mut *holder)
    .await
    .unwrap();

    let mut contender = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL DateStyle = 'ISO, MDY'")
        .execute(&mut *contender)
        .await
        .unwrap();
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended(\
             'retained_response_objects.partition:' || current_schema() || ':'\
                 || to_char($1::date, 'YYYYMMDD'), 0))",
    )
    .bind(today)
    .fetch_one(&mut *contender)
    .await
    .unwrap();
    assert!(
        !acquired,
        "all DateStyle settings must address one lock key"
    );
    contender.rollback().await.unwrap();
    holder.rollback().await.unwrap();
}

#[sqlx::test]
async fn database_utc_date_retires_today_but_never_tomorrow(pool: PgPool) {
    let (today, tomorrow): (chrono::NaiveDate, chrono::NaiveDate) = sqlx::query_as(
        "SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date, \
                (statement_timestamp() AT TIME ZONE 'UTC')::date + 1",
    )
    .fetch_one(&pool)
    .await
    .expect("database UTC date must be readable");
    for delete_on in [today, tomorrow] {
        ensure_partition(&pool, delete_on).await;
    }

    let manager = retirement_manager(&pool).await;
    assert_eq!(
        manager
            .retire_expired_response_partition(true)
            .await
            .expect("today's bucket must retire"),
        RetainedResponseRetirementOutcome::Retired,
    );
    assert_eq!(
        manager
            .retire_expired_response_partition(true)
            .await
            .expect("tomorrow must not be selected"),
        RetainedResponseRetirementOutcome::NoCandidate,
    );

    let states: Vec<(chrono::NaiveDate, String)> =
        sqlx::query_as("SELECT delete_on, state FROM retained_response_buckets ORDER BY delete_on")
            .fetch_all(&pool)
            .await
            .expect("bucket states must be readable");
    assert_eq!(
        states,
        vec![
            (today, "retired".to_string()),
            (tomorrow, "active".to_string())
        ]
    );
}

#[sqlx::test]
async fn unfinished_journal_recovers_when_new_selection_is_disabled(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let manager = retirement_manager(&pool).await;

    assert_eq!(
        manager
            .retire_expired_response_partition(false)
            .await
            .expect("disabled selection must still recover durable work"),
        RetainedResponseRetirementOutcome::Retired
    );
    let completion: (
        String,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        r#"
            SELECT bucket.state, bucket.state_changed_at, journal.completed_at
            FROM retained_response_buckets bucket
            JOIN retention_partition_retirements journal
              ON journal.parent_table = 'retained_response_objects'
             AND journal.partition_table = bucket.partition_table
            WHERE bucket.delete_on = $1
            "#,
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .expect("completion proof must remain");
    assert_eq!(completion.0, "retired");
    assert_eq!(completion.1, completion.2);
    let relation: Option<i64> = sqlx::query_scalar(
        "SELECT to_regclass(format('%I.%I', current_schema(), $1))::oid::bigint",
    )
    .bind(partition_name(today))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(relation, None);
}

#[sqlx::test]
async fn a_retiring_bucket_without_its_recovery_journal_fails_closed(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(today)
        .execute(&pool)
        .await
        .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("an orphan read fence has no safe recovery proof");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
}

#[sqlx::test]
async fn replaced_partition_identity_fails_before_destructive_ddl(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    let original_oid: i64 = sqlx::query_scalar(
        "SELECT partition_oid::bigint FROM retained_response_buckets WHERE delete_on = $1",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    let name = partition_name(today);
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects DETACH PARTITION {name}"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!("ALTER TABLE {name} RENAME TO {name}_old"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {name} (LIKE retained_response_objects INCLUDING ALL)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects ATTACH PARTITION {name} \
         FOR VALUES FROM ('{today}') TO ('{}')",
        today.succ_opt().unwrap()
    ))
    .execute(&pool)
    .await
    .unwrap();

    let manager = retirement_manager(&pool).await;
    let error = manager
        .retire_expired_response_partition(true)
        .await
        .expect_err("same name with a different OID must fail closed");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM retained_response_buckets WHERE delete_on = $1",
        )
        .bind(today)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "active"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM retention_partition_retirements \
             WHERE parent_table = 'retained_response_objects'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT oid::bigint FROM pg_class WHERE oid = $1::oid",)
            .bind(original_oid as i32)
            .fetch_one(&pool)
            .await
            .unwrap(),
        original_oid
    );
}

#[sqlx::test]
async fn a_valid_partition_in_another_schema_is_never_in_scope(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    let child = partition_name(today);
    sqlx::query("CREATE SCHEMA adversarial_retirement")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE adversarial_retirement.retained_response_objects (delete_on DATE NOT NULL) \
         PARTITION BY RANGE (delete_on)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE adversarial_retirement.{child} \
         PARTITION OF adversarial_retirement.retained_response_objects \
         FOR VALUES FROM ('{today}') TO ('{}')",
        today.succ_opt().unwrap()
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO retained_response_buckets (
            delete_on, partition_schema, partition_table, partition_oid, state
        )
        SELECT $1, namespace.nspname, child.relname, child.oid, 'retiring'
        FROM pg_class child
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        WHERE namespace.nspname = 'adversarial_retirement'
          AND child.relname = $2
        "#,
    )
    .bind(today)
    .bind(&child)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO retention_partition_retirements (
            parent_table, partition_table, partition_oid,
            partition_schema, partition_schema_oid, parent_oid,
            lower_bound, upper_bound
        )
        SELECT 'retained_response_objects', child.relname, child.oid,
               namespace.nspname, namespace.oid, parent.oid, $1, $1 + 1
        FROM pg_class child
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        JOIN pg_class parent
          ON parent.relnamespace = namespace.oid
         AND parent.relname = 'retained_response_objects'
        WHERE namespace.nspname = 'adversarial_retirement'
          AND child.relname = $2
        "#,
    )
    .bind(today)
    .bind(&child)
    .execute(&pool)
    .await
    .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("another schema is outside this manager's scope");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    let proof: (bool, Option<Uuid>, Option<chrono::DateTime<chrono::Utc>>, String) =
        sqlx::query_as(
            r#"
            SELECT EXISTS (
                       SELECT 1 FROM pg_inherits
                       WHERE inhparent = 'adversarial_retirement.retained_response_objects'::regclass
                         AND inhrelid = format('adversarial_retirement.%I', $1)::regclass
                   ),
                   journal.lease_owner, journal.completed_at, bucket.state
            FROM retention_partition_retirements journal
            JOIN retained_response_buckets bucket ON bucket.delete_on = journal.lower_bound
            WHERE journal.parent_table = 'retained_response_objects'
            "#,
        )
        .bind(&child)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(proof, (true, None, None, "retiring".to_string()));
}

#[sqlx::test]
async fn concurrent_detach_refuses_a_parent_with_a_default_partition(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    sqlx::query(
        "CREATE TABLE retained_response_objects_default \
         PARTITION OF retained_response_objects DEFAULT",
    )
    .execute(&pool)
    .await
    .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(true)
        .await
        .expect_err("concurrent detach cannot safely fall back around a default partition");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    let proof: (String, i64) = sqlx::query_as(
        "SELECT bucket.state, COUNT(journal.*) \
         FROM retained_response_buckets bucket \
         LEFT JOIN retention_partition_retirements journal \
           ON journal.parent_table = 'retained_response_objects' \
         WHERE bucket.delete_on = $1 GROUP BY bucket.state",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proof, ("active".to_string(), 0));
}

#[sqlx::test]
async fn an_exact_detached_journal_resumes_at_drop(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let child = partition_name(today);
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects DETACH PARTITION {child}"
    ))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        retirement_manager(&pool)
            .await
            .retire_expired_response_partition(false)
            .await
            .expect("an exact detached generation must resume at finalization"),
        RetainedResponseRetirementOutcome::Retired
    );
}

#[sqlx::test]
async fn a_missing_child_is_an_error_while_the_journal_is_pending(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let child = partition_name(today);
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects DETACH PARTITION {child}"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!("DROP TABLE {child}"))
        .execute(&pool)
        .await
        .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("absence cannot stand in for durable completion proof");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    let proof: (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT bucket.state, journal.completed_at \
         FROM retained_response_buckets bucket \
         JOIN retention_partition_retirements journal \
           ON journal.lower_bound = bucket.delete_on \
         WHERE bucket.delete_on = $1",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proof, ("retiring".to_string(), None));
}

#[sqlx::test]
async fn a_pending_journal_with_the_wrong_parent_oid_fails_closed(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    sqlx::query(
        "UPDATE retention_partition_retirements \
         SET parent_oid = 'retained_response_buckets'::regclass \
         WHERE parent_table = 'retained_response_objects'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("a different parent generation must never be altered");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM retained_response_buckets WHERE delete_on = $1",
        )
        .bind(today)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "retiring"
    );
}

#[sqlx::test]
async fn a_journal_bounds_tampering_write_is_rejected_by_the_schema(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let tampering = sqlx::query(
        "UPDATE retention_partition_retirements \
         SET lower_bound = lower_bound - 1, upper_bound = upper_bound - 1 \
         WHERE parent_table = 'retained_response_objects'",
    )
    .execute(&pool)
    .await
    .expect_err("a journal row whose bounds disagree with its child name must be unrepresentable");
    assert_eq!(
        tampering
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
}

#[sqlx::test]
async fn a_pending_journal_with_shifted_catalog_bounds_fails_closed(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    // The identity-complete constraint makes journal-side bounds tampering
    // unrepresentable, so the reachable mismatch is catalog-side: the exact
    // journaled child OID now spans a different daily range than it did when
    // the retirement was claimed.
    let child = partition_name(today);
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects DETACH PARTITION {child}"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "ALTER TABLE retained_response_objects ATTACH PARTITION {child} \
         FOR VALUES FROM ('{}') TO ('{}')",
        today - chrono::Duration::days(1),
        today
    ))
    .execute(&pool)
    .await
    .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("shifted daily bounds must never address the journaled child");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE oid = \
             (SELECT partition_oid FROM retained_response_buckets WHERE delete_on = $1))",
        )
        .bind(today)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}

#[sqlx::test]
async fn a_renamed_pending_child_fails_closed_without_touching_its_oid(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let child = partition_name(today);
    let renamed = format!("{child}_renamed");
    sqlx::query(&format!("ALTER TABLE {child} RENAME TO {renamed}"))
        .execute(&pool)
        .await
        .unwrap();

    let error = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect_err("name drift must never redirect destructive DDL");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT to_regclass(format('%I.%I', current_schema(), $1)) IS NOT NULL",
        )
        .bind(renamed)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}

#[sqlx::test]
async fn a_renamed_parent_fails_closed_without_touching_the_child(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let manager = retirement_manager(&pool).await;
    sqlx::query("ALTER TABLE retained_response_objects RENAME TO displaced_response_parent")
        .execute(&pool)
        .await
        .unwrap();

    let error = manager
        .retire_expired_response_partition(false)
        .await
        .expect_err("a missing canonical parent must stop recovery");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE oid = \
             (SELECT partition_oid FROM retained_response_buckets WHERE delete_on = $1))",
        )
        .bind(today)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}

#[sqlx::test]
async fn a_live_foreign_lease_waits_but_an_expired_lease_is_recoverable(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let foreign_owner = Uuid::new_v4();
    sqlx::query(
        "UPDATE retention_partition_retirements \
         SET lease_owner = $1, lease_expires_at = statement_timestamp() + INTERVAL '1 hour' \
         WHERE parent_table = 'retained_response_objects'",
    )
    .bind(foreign_owner)
    .execute(&pool)
    .await
    .unwrap();
    let manager = retirement_manager(&pool).await;
    assert_eq!(
        manager
            .retire_expired_response_partition(false)
            .await
            .unwrap(),
        RetainedResponseRetirementOutcome::Retryable
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT lease_owner FROM retention_partition_retirements \
             WHERE parent_table = 'retained_response_objects'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        Some(foreign_owner)
    );
    sqlx::query(
        "UPDATE retention_partition_retirements \
         SET lease_expires_at = statement_timestamp() - INTERVAL '1 second' \
         WHERE parent_table = 'retained_response_objects'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        manager
            .retire_expired_response_partition(false)
            .await
            .expect("an expired foreign lease must be reclaimable"),
        RetainedResponseRetirementOutcome::Retired
    );
}

#[sqlx::test]
async fn the_same_manager_retries_immediately_after_lock_timeout(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    let child = partition_name(today);
    let mut blocker = pool.acquire().await.unwrap();
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::query(&format!("SELECT COUNT(*) FROM {child}"))
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let manager = retirement_manager(&pool).await;

    assert_eq!(
        manager
            .retire_expired_response_partition(true)
            .await
            .expect("a server lock timeout is a retryable no-op"),
        RetainedResponseRetirementOutcome::Retryable
    );
    let first_owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT lease_owner FROM retention_partition_retirements \
         WHERE parent_table = 'retained_response_objects'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(first_owner.is_some());
    let detach_pending: bool = sqlx::query_scalar(
        "SELECT inheritance.inhdetachpending \
         FROM pg_inherits inheritance \
         WHERE inheritance.inhparent = 'retained_response_objects'::regclass \
           AND inheritance.inhrelid = $1::regclass",
    )
    .bind(&child)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        detach_pending,
        "the timed-out detach must remain recoverable"
    );
    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .unwrap();

    assert_eq!(
        manager
            .retire_expired_response_partition(false)
            .await
            .expect("the stable owner must renew its own live lease"),
        RetainedResponseRetirementOutcome::Retired
    );
}

#[sqlx::test]
async fn journal_row_lock_timeout_is_a_retryable_no_op(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    install_pending_journal(&pool, today).await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM retention_partition_retirements \
         WHERE parent_table = 'retained_response_objects' AND completed_at IS NULL \
         FOR UPDATE",
    )
    .execute(&mut *blocker)
    .await
    .unwrap();

    let outcome = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(false)
        .await
        .expect("row-lock timeout must remain content-free and retryable");
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retryable);
    blocker.rollback().await.unwrap();
    let proof: (String, bool) = sqlx::query_as(
        "SELECT bucket.state, journal.completed_at IS NULL \
         FROM retained_response_buckets bucket \
         JOIN retention_partition_retirements journal \
           ON journal.lower_bound = bucket.delete_on \
         WHERE bucket.delete_on = $1",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proof, ("retiring".to_string(), true));
}

#[sqlx::test]
async fn bucket_row_lock_timeout_does_not_create_a_journal(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM retained_response_buckets WHERE delete_on = $1 FOR UPDATE")
        .bind(today)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let outcome = retirement_manager(&pool)
        .await
        .retire_expired_response_partition(true)
        .await
        .expect("bucket row-lock timeout must remain content-free and retryable");
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retryable);
    blocker.rollback().await.unwrap();
    let proof: (String, i64) = sqlx::query_as(
        "SELECT bucket.state, COUNT(journal.*) \
         FROM retained_response_buckets bucket \
         LEFT JOIN retention_partition_retirements journal \
           ON journal.lower_bound = bucket.delete_on \
         WHERE bucket.delete_on = $1 GROUP BY bucket.state",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proof, ("active".to_string(), 0));
}

#[sqlx::test]
async fn retirement_configures_server_side_session_timeouts(pool: PgPool) {
    let maintenance = maintenance_pool(&pool).await;
    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600))
    .with_partition_maintenance_pool(maintenance.clone())
    .unwrap()
    .attest_partition_maintenance_pool()
    .await
    .unwrap();
    assert_eq!(
        manager
            .retire_expired_response_partition(false)
            .await
            .unwrap(),
        RetainedResponseRetirementOutcome::NoCandidate
    );
    let settings: (String, String) = sqlx::query_as(
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
    )
    .fetch_one(&maintenance)
    .await
    .unwrap();
    assert_eq!(settings, ("5s".to_string(), "30s".to_string()));
}

#[sqlx::test]
async fn concurrent_retirees_have_exactly_one_winner(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    let first = retirement_manager(&pool).await;
    let second = retirement_manager(&pool).await;
    let (first_outcome, second_outcome) = tokio::join!(
        first.retire_expired_response_partition(true),
        second.retire_expired_response_partition(true),
    );
    let outcomes = [first_outcome.unwrap(), second_outcome.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == RetainedResponseRetirementOutcome::Retired)
            .count(),
        1
    );
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        RetainedResponseRetirementOutcome::Retired
            | RetainedResponseRetirementOutcome::Retryable
            | RetainedResponseRetirementOutcome::NoCandidate
    )));
    let proof: (String, i64, i64) = sqlx::query_as(
        "SELECT bucket.state, \
                COUNT(*) FILTER (WHERE journal.completed_at IS NOT NULL), \
                COUNT(*) \
         FROM retained_response_buckets bucket \
         JOIN retention_partition_retirements journal \
           ON journal.lower_bound = bucket.delete_on \
         WHERE bucket.delete_on = $1 GROUP BY bucket.state",
    )
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proof, ("retired".to_string(), 1, 1));
}

#[sqlx::test]
async fn route_cleanup_is_bounded_and_fences_every_identifier_before_deletion(pool: PgPool) {
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (statement_timestamp() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    ensure_partition(&pool, today).await;
    let manager = retirement_manager(&pool).await;
    assert_eq!(
        manager
            .retire_expired_response_partition(true)
            .await
            .unwrap(),
        RetainedResponseRetirementOutcome::Retired
    );

    let groups = [Uuid::new_v4(), Uuid::new_v4()];
    let requests = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let steps = [Uuid::new_v4()];
    sqlx::query(
        "INSERT INTO retained_response_group_routes (group_id, delete_on) \
         SELECT * FROM UNNEST($1::uuid[], $2::date[])",
    )
    .bind(groups.to_vec())
    .bind(vec![today; groups.len()])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO retained_response_request_routes (request_id, group_id, delete_on) \
         SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::date[])",
    )
    .bind(requests.to_vec())
    .bind(vec![groups[0], groups[0], groups[1]])
    .bind(vec![today; requests.len()])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO retained_response_step_routes (step_id, group_id, delete_on) \
         VALUES ($1, $2, $3)",
    )
    .bind(steps[0])
    .bind(groups[0])
    .bind(today)
    .execute(&pool)
    .await
    .unwrap();
    let erased_expiry: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() + INTERVAL '1 day'")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO retained_response_resurrection_fences \
         (object_id, reason, expires_at) VALUES ($1, 'erased', $2)",
    )
    .bind(requests[0])
    .bind(erased_expiry)
    .execute(&pool)
    .await
    .unwrap();

    let mut deleted = 0_u64;
    loop {
        let chunk = manager
            .cleanup_retained_response_routes(2)
            .await
            .expect("route cleanup must remain independently retryable");
        assert!(chunk <= 2, "a cleanup chunk exceeded its bound");
        if chunk == 0 {
            break;
        }
        deleted += chunk;
    }
    assert_eq!(deleted, 6);
    for relation in [
        "retained_response_request_routes",
        "retained_response_step_routes",
        "retained_response_group_routes",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {relation}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let fences: Vec<(
        Uuid,
        String,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"
            SELECT fence.object_id, fence.reason, fence.expires_at,
                   journal.completed_at + INTERVAL '1 hour' AS canonical_expiry
            FROM retained_response_resurrection_fences fence
            CROSS JOIN retention_partition_retirements journal
            WHERE journal.parent_table = 'retained_response_objects'
              AND journal.lower_bound = $1
            ORDER BY fence.object_id
            "#,
    )
    .bind(today)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(fences.len(), 6);
    for (object_id, reason, expiry, canonical_expiry) in fences {
        if object_id == requests[0] {
            assert_eq!(reason, "erased");
            assert_eq!(expiry, erased_expiry);
        } else {
            assert_eq!(reason, "retired");
            assert_eq!(expiry, canonical_expiry);
        }
    }
}

async fn install_fence(pool: &PgPool, id: Uuid, reason: &str, expired: bool) {
    let offset = if expired {
        "- INTERVAL '1 second'"
    } else {
        "+ INTERVAL '1 hour'"
    };
    sqlx::query(&format!(
        "INSERT INTO retained_response_resurrection_fences (object_id, reason, expires_at) \
         VALUES ($1, $2, statement_timestamp() {offset})",
    ))
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await
    .expect("fence fixture must insert");
}

#[sqlx::test]
async fn expired_fence_cleanup_is_bounded_and_idempotent(pool: PgPool) {
    let manager = retirement_manager(&pool).await;
    let expired: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    for id in &expired {
        install_fence(&pool, *id, "archived", true).await;
    }
    let live = Uuid::new_v4();
    install_fence(&pool, live, "erased", false).await;

    assert_eq!(manager.cleanup_expired_response_fences(2).await.unwrap(), 2);
    assert_eq!(manager.cleanup_expired_response_fences(2).await.unwrap(), 1);
    assert_eq!(manager.cleanup_expired_response_fences(2).await.unwrap(), 0);
    let survivors: Vec<Uuid> = sqlx::query_scalar(
        "SELECT object_id FROM retained_response_resurrection_fences ORDER BY object_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(survivors, vec![live]);
}

#[sqlx::test]
async fn expired_fence_cleanup_rechecks_a_concurrent_renewal_at_deletion_time(pool: PgPool) {
    let manager = retirement_manager(&pool).await;
    let contended = Uuid::new_v4();
    install_fence(&pool, contended, "archived", true).await;

    // A concurrent destructive lifecycle action holds the fence row lock and
    // upgrades/renews it. SKIP LOCKED must skip the row, and after the renewal
    // commits the deletion-time recheck must observe the extended expiry.
    let mut renewal = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE retained_response_resurrection_fences \
         SET reason = 'erased', \
             expires_at = GREATEST(expires_at, statement_timestamp() + INTERVAL '1 hour') \
         WHERE object_id = $1",
    )
    .bind(contended)
    .execute(&mut *renewal)
    .await
    .unwrap();
    assert_eq!(
        manager.cleanup_expired_response_fences(10).await.unwrap(),
        0
    );
    renewal.commit().await.unwrap();

    assert_eq!(
        manager.cleanup_expired_response_fences(10).await.unwrap(),
        0
    );
    let (reason, live): (String, bool) = sqlx::query_as(
        "SELECT reason, expires_at > statement_timestamp() \
         FROM retained_response_resurrection_fences WHERE object_id = $1",
    )
    .bind(contended)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason, "erased");
    assert!(
        live,
        "a renewed explicit-erasure fence must survive cleanup"
    );
}

#[sqlx::test]
async fn expired_fence_cleanup_validates_its_bound(pool: PgPool) {
    let manager = retirement_manager(&pool).await;
    assert!(manager.cleanup_expired_response_fences(-1).await.is_err());
    assert_eq!(manager.cleanup_expired_response_fences(0).await.unwrap(), 0);
}
