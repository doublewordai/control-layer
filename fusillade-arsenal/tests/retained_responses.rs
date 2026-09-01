use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use fusillade_arsenal::batch::TemplateId;
use fusillade_arsenal::manager::{
    RetainedResponseArchiveCutoffs, RetainedResponseArchiveOutcome,
    RetainedResponseMaintenanceError, RetainedResponseWriteError, RetentionPolicy,
};
use fusillade_arsenal::postgres_response_step::{
    ResponseStepNotFound, RetainedResponseStepConflict,
};
use fusillade_arsenal::request::{
    Completed, CreateRealtimeInput, DaemonId, ListRequestsFilter, PersistCompletedRealtimeInput,
    Request, RequestData, RequestId, ServiceTierFilter,
};
use fusillade_arsenal::response_step::{CreateStepInput, ResponseStepStore, StepId, StepKind};
use fusillade_arsenal::{
    DaemonStorage, PoolProvider, PostgresRequestManager, PostgresResponseStepManager,
    PostgresStorageConfig, Storage, TestDbPools,
};
use serde_json::{Value, json};
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::Barrier;
use uuid::Uuid;

const MODEL: &str = "retention-test-model";
const OWNER: &str = "retention-test-owner";
// Historical movement fixtures use a frozen observation time and a visible
// policy offset so they always target a future partition. Exact retention
// boundaries use `exact_policy` and compute their partition date directly.
const FIXTURE_RETENTION_OFFSET_DAYS: i64 = 30;
const FIXTURE_RETENTION_OFFSET_SECONDS: u64 = FIXTURE_RETENTION_OFFSET_DAYS as u64 * 86_400;

#[derive(Clone, Copy)]
enum TerminalState {
    Completed,
    Failed,
    Canceled { dispatched: bool },
    Pending,
}

struct LiveGraph {
    group_id: Uuid,
    request_ids: Vec<Uuid>,
    step_ids: Vec<Uuid>,
    template_ids: Vec<Uuid>,
}

struct StepFixture {
    id: Uuid,
    request_id: Option<Uuid>,
    prev_step_id: Option<Uuid>,
    parent_step_id: Option<Uuid>,
    sequence: i64,
    state: TerminalState,
    terminal_at: DateTime<Utc>,
}

#[derive(Clone)]
struct WriteSignalingPools {
    read: PgPool,
    write: PgPool,
    write_requested: Arc<AtomicBool>,
}

#[derive(Clone)]
struct DiscoveryReadGatedPools {
    read: PgPool,
    gated_read: PgPool,
    read_selections: Arc<AtomicUsize>,
    gate_requested: Arc<AtomicBool>,
}

impl PoolProvider for DiscoveryReadGatedPools {
    fn read(&self) -> &PgPool {
        // Zero-based selection 2 is the bounded look-ahead after the index
        // readiness read and the first atomic candidate snapshot. The archive
        // clock is supplied immutably by the caller, so it consumes no pool
        // selection.
        if self.read_selections.fetch_add(1, Ordering::AcqRel) == 2 {
            self.gate_requested.store(true, Ordering::Release);
            &self.gated_read
        } else {
            &self.read
        }
    }

    fn write(&self) -> &PgPool {
        self.gate_requested.store(true, Ordering::Release);
        &self.read
    }
}

impl PoolProvider for WriteSignalingPools {
    fn read(&self) -> &PgPool {
        &self.read
    }

    fn write(&self) -> &PgPool {
        self.write_requested.store(true, Ordering::Release);
        &self.write
    }
}

fn timestamp(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp must be valid")
        .to_utc()
}

fn archive_date(value: &str) -> NaiveDate {
    exact_date(value)
        .checked_add_signed(TimeDelta::days(FIXTURE_RETENTION_OFFSET_DAYS))
        .expect("fixture date offset must be valid")
}

fn exact_date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("fixture date must be valid")
}

fn collect_plan_nodes<'a>(node: &'a Value, nodes: &mut Vec<&'a serde_json::Map<String, Value>>) {
    let Some(object) = node.as_object() else {
        return;
    };
    nodes.push(object);
    if let Some(children) = object.get("Plans").and_then(Value::as_array) {
        for child in children {
            collect_plan_nodes(child, nodes);
        }
    }
}

fn policy(tiers: &[(&str, u64)]) -> RetentionPolicy {
    exact_policy(
        &tiers
            .iter()
            .map(|(tier, seconds)| {
                (
                    *tier,
                    seconds
                        .checked_add(FIXTURE_RETENTION_OFFSET_SECONDS)
                        .expect("fixture retention duration must be valid"),
                )
            })
            .collect::<Vec<_>>(),
    )
}

fn exact_policy(tiers: &[(&str, u64)]) -> RetentionPolicy {
    RetentionPolicy {
        max_late_writer_seconds: Some(3_600),
        batchless_seconds_by_service_tier: tiers
            .iter()
            .map(|(tier, seconds)| ((*tier).to_owned(), *seconds))
            .collect::<HashMap<_, _>>(),
        ..Default::default()
    }
}

async fn assert_wholly_erased(pool: &PgPool, graph: &LiveGraph) {
    assert_eq!(count_ids(pool, "requests", &graph.request_ids).await, 0);
    assert_eq!(count_ids(pool, "response_steps", &graph.step_ids).await, 0);
    assert_eq!(
        count_ids(pool, "request_templates", &graph.template_ids).await,
        0
    );
    assert_eq!(retained_counts(pool, graph.group_id).await, (0, 0, 0));
    let route_count: i64 = sqlx::query_scalar(
        r#"
        SELECT (SELECT COUNT(*) FROM retained_response_group_routes WHERE group_id = $1)
             + (SELECT COUNT(*) FROM retained_response_request_routes WHERE group_id = $1)
             + (SELECT COUNT(*) FROM retained_response_step_routes WHERE group_id = $1)
        "#,
    )
    .bind(graph.group_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(route_count, 0);
    let fenced: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_resurrection_fences \
         WHERE object_id = ANY($1) AND reason = 'erased' AND expires_at > NOW()",
    )
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let expected = std::iter::once(graph.group_id)
        .chain(graph.request_ids.iter().copied())
        .chain(graph.step_ids.iter().copied())
        .collect::<std::collections::HashSet<_>>()
        .len() as i64;
    assert_eq!(fenced, expected);
}

#[sqlx::test]
async fn erase_retained_singleton_by_request_id_removes_the_complete_graph(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-singleton",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();

    assert_eq!(
        manager
            .delete_response_group(graph.request_ids[0])
            .await
            .unwrap(),
        1
    );
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn erase_retained_branching_graph_by_head_or_member_is_atomic(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let manager = manager(&pool).await;
    archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();

    assert_eq!(
        manager
            .delete_response_group(graph.request_ids[1])
            .await
            .unwrap(),
        2
    );
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn erase_never_shortens_an_existing_fence_expiry(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "fence-expiry-monotonic",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();
    // Truncate to PostgreSQL's microsecond resolution: a nanosecond remainder
    // would make the round-tripped fence expiry compare strictly smaller.
    let protected_until = chrono::SubsecRound::trunc_subsecs(Utc::now() + TimeDelta::days(2), 6);
    sqlx::query(
        "UPDATE retained_response_resurrection_fences SET expires_at = $1 \
         WHERE object_id = ANY($2)",
    )
    .bind(protected_until)
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .execute(&pool)
    .await
    .unwrap();

    manager.delete_response_group(graph.group_id).await.unwrap();

    let fences: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT reason, expires_at FROM retained_response_resurrection_fences \
         WHERE object_id = ANY($1)",
    )
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!fences.is_empty());
    assert!(
        fences
            .iter()
            .all(|(reason, expiry)| reason == "erased" && *expiry >= protected_until),
        "a later lifecycle event may advance the reason but never shorten expiry"
    );
}

#[sqlx::test]
async fn erase_creator_retained_groups_in_stable_bounded_units(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let branching = branching_graph(&pool, TerminalState::Completed).await;
    let singleton = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "creator-second-group",
    )
    .await;
    let manager = manager(&pool).await;
    archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        2,
        i64::MAX,
    )
    .await
    .unwrap();

    let first = manager.bulk_delete_data(OWNER, 1).await.unwrap();
    assert!(matches!(first, 1 | 2));
    let remaining = [
        retained_counts(&pool, branching.group_id).await.0,
        retained_counts(&pool, singleton.group_id).await.0,
    ]
    .into_iter()
    .sum::<i64>();
    assert_eq!(remaining, 1, "one whole group must remain after one unit");

    let second = manager.bulk_delete_data(OWNER, 1).await.unwrap();
    assert!(matches!(second, 1 | 2));
    assert_eq!(manager.bulk_delete_data(OWNER, 1).await.unwrap(), 0);
    assert_wholly_erased(&pool, &branching).await;
    assert_wholly_erased(&pool, &singleton).await;
}

#[sqlx::test]
async fn creator_erasure_selects_the_oldest_live_graph_before_aggregation(pool: PgPool) {
    let oldest = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "creator-oldest",
    )
    .await;
    let middle = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "creator-middle",
    )
    .await;
    let newest = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "creator-newest",
    )
    .await;
    let manager = manager(&pool).await;

    assert_eq!(manager.bulk_delete_data(OWNER, 1).await.unwrap(), 1);
    assert_wholly_erased(&pool, &oldest).await;
    assert_wholly_live(&pool, &middle).await;
    assert_wholly_live(&pool, &newest).await;
}

#[sqlx::test]
async fn creator_erasure_retries_an_orphan_retiring_fence(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "creator-retiring",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE retained_response_buckets SET state = 'retiring' \
         WHERE delete_on = $1",
    )
    .bind(archive_date("2026-08-03"))
    .execute(&pool)
    .await
    .unwrap();

    let error = manager
        .bulk_delete_data(OWNER, 1)
        .await
        .expect_err("an orphan retiring fence cannot certify physical deletion");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::RetirementPending)
    );
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn creator_erasure_reports_progress_when_oldest_graph_is_busy(pool: PgPool) {
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "creator-busy",
    )
    .await;
    let manager = manager(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended( \
         'retained_response_graph:' || current_schema() || ':' || $1::text, 0))",
    )
    .bind(graph.group_id)
    .execute(&mut *holder)
    .await
    .unwrap();

    assert_eq!(
        manager.bulk_delete_data(OWNER, 1).await.unwrap(),
        1,
        "temporary contention must not look like a fully drained creator"
    );
    assert_wholly_live(&pool, &graph).await;
    holder.commit().await.unwrap();
    assert_eq!(manager.bulk_delete_data(OWNER, 1).await.unwrap(), 1);
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn direct_erasure_restarts_when_a_singleton_gains_a_head_while_waiting(pool: PgPool) {
    let mut graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-canonical-lock-race",
    )
    .await;
    let request_id = graph.request_ids[0];
    let head_id = Uuid::new_v4();

    let mut singleton_locker = pool.acquire().await.unwrap();
    let singleton_locker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *singleton_locker)
        .await
        .unwrap();
    lock_session_response_graph(&mut singleton_locker, request_id).await;

    let erase_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let erase_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&erase_pool)
        .await
        .unwrap();
    let erase_manager = manager(&erase_pool).await;
    let mut eraser =
        tokio::spawn(async move { erase_manager.delete_response_group(request_id).await });
    tokio::select! {
        result = &mut eraser => panic!("erasure completed before the tentative singleton lock was released: {result:?}"),
        () = wait_for_backend_blocked_by(&pool, erase_pid, singleton_locker_pid) => {}
    }

    insert_step(
        &pool,
        StepFixture {
            id: head_id,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 0,
            state: TerminalState::Pending,
            terminal_at: timestamp("2026-08-01T10:01:00Z"),
        },
    )
    .await;
    graph.group_id = head_id;
    graph.step_ids.push(head_id);

    let mut head_locker = pool.acquire().await.unwrap();
    let head_locker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *head_locker)
        .await
        .unwrap();
    lock_session_response_graph(&mut head_locker, head_id).await;
    unlock_session_response_graph(&mut singleton_locker, request_id).await;

    tokio::select! {
        result = &mut eraser => panic!("erasure used the obsolete singleton lock instead of restarting under the headed graph: {result:?}"),
        () = wait_for_backend_blocked_by(&pool, erase_pid, head_locker_pid) => {}
    }
    unlock_session_response_graph(&mut head_locker, head_id).await;

    assert_eq!(eraser.await.unwrap().unwrap(), 1);
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn request_writer_restarts_when_a_singleton_gains_a_head_while_waiting(pool: PgPool) {
    let mut graph = singleton(
        &pool,
        "flex",
        TerminalState::Pending,
        timestamp("2026-08-01T10:00:00Z"),
        "writer-canonical-lock-race",
    )
    .await;
    let request_id = graph.request_ids[0];
    let head_id = Uuid::new_v4();

    let mut singleton_locker = pool.acquire().await.unwrap();
    let singleton_locker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *singleton_locker)
        .await
        .unwrap();
    lock_session_response_graph(&mut singleton_locker, request_id).await;

    let writer_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&writer_pool)
        .await
        .unwrap();
    let writer_manager = manager(&writer_pool).await;
    let terminal = generic_terminal_write(&graph);
    let mut writer = tokio::spawn(async move { writer_manager.persist(&terminal).await });
    tokio::select! {
        result = &mut writer => panic!("writer completed before the tentative singleton lock was released: {result:?}"),
        () = wait_for_backend_blocked_by(&pool, writer_pid, singleton_locker_pid) => {}
    }

    insert_step(
        &pool,
        StepFixture {
            id: head_id,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 0,
            state: TerminalState::Pending,
            terminal_at: timestamp("2026-08-01T10:01:00Z"),
        },
    )
    .await;
    graph.group_id = head_id;
    graph.step_ids.push(head_id);

    let mut head_locker = pool.acquire().await.unwrap();
    let head_locker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *head_locker)
        .await
        .unwrap();
    lock_session_response_graph(&mut head_locker, head_id).await;
    unlock_session_response_graph(&mut singleton_locker, request_id).await;

    tokio::select! {
        result = &mut writer => panic!("writer used the obsolete singleton lock instead of restarting under the headed graph: {result:?}"),
        () = wait_for_backend_blocked_by(&pool, writer_pid, head_locker_pid) => {}
    }
    unlock_session_response_graph(&mut head_locker, head_id).await;

    writer.await.unwrap().unwrap();
    assert_wholly_live(&pool, &graph).await;
    let state: String = sqlx::query_scalar("SELECT state FROM requests WHERE id = $1")
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "completed");
}

#[sqlx::test]
async fn retained_creator_seed_uses_bounded_owner_order_index_under_default_planner(pool: PgPool) {
    ensure_partition(&pool, exact_date("2026-08-03")).await;
    ensure_partition(&pool, exact_date("2026-08-07")).await;

    sqlx::query(
        r#"
        INSERT INTO retained_response_objects (
            delete_on, group_id, object_kind, object_id, created_by,
            created_at, schema_version, payload
        )
        SELECT fixture.delete_on, fixture.object_id, 'request', fixture.object_id,
               $1, fixture.created_at, 1, '{}'::jsonb
        FROM (VALUES
            ('2026-08-07'::date, '00000000-0000-0000-0000-000000000001'::uuid,
             '2026-08-01T08:00:00Z'::timestamptz),
            ('2026-08-03'::date, '00000000-0000-0000-0000-000000000002'::uuid,
             '2026-08-01T08:00:00Z'::timestamptz),
            ('2026-08-07'::date, '00000000-0000-0000-0000-000000000003'::uuid,
             '2026-08-01T09:00:00Z'::timestamptz),
            ('2026-08-03'::date, '00000000-0000-0000-0000-000000000004'::uuid,
             '2026-08-01T10:00:00Z'::timestamptz)
        ) AS fixture(delete_on, object_id, created_at)
        "#,
    )
    .bind(OWNER)
    .execute(&pool)
    .await
    .unwrap();
    for (delete_on, prefix) in [
        (exact_date("2026-08-03"), "creator-plan-a-"),
        (exact_date("2026-08-07"), "creator-plan-b-"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO retained_response_objects (
                delete_on, group_id, object_kind, object_id, created_by,
                created_at, schema_version, payload
            )
            SELECT $1, generated.object_id, 'request', generated.object_id,
                   'different-owner',
                   '2026-08-01T00:00:00Z'::timestamptz
                       + generated.ordinal * interval '1 second',
                   1, '{}'::jsonb
            FROM (
                SELECT ordinal, md5($2 || ordinal::text)::uuid AS object_id
                FROM generate_series(1, 10000) ordinal
            ) generated
            "#,
        )
        .bind(delete_on)
        .bind(prefix)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("ANALYZE retained_response_objects")
        .execute(&pool)
        .await
        .unwrap();

    let expected = [
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
    ];
    let selected: Vec<Uuid> = sqlx::query_scalar(
        "SELECT object.object_id \
         FROM retained_response_objects object \
         JOIN retained_response_buckets bucket ON bucket.delete_on = object.delete_on \
          AND bucket.state IN ('active', 'retiring') \
         WHERE object.object_kind = 'request' AND object.created_by = $1 \
         ORDER BY object.created_at, object.object_id LIMIT 3",
    )
    .bind(OWNER)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        selected, expected,
        "retained seeds must be exactly oldest-first"
    );

    let explained: Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, FORMAT JSON) \
         SELECT object.object_id \
         FROM retained_response_objects object \
         JOIN retained_response_buckets bucket ON bucket.delete_on = object.delete_on \
          AND bucket.state IN ('active', 'retiring') \
         WHERE object.object_kind = 'request' AND object.created_by = $1 \
         ORDER BY object.created_at, object.object_id LIMIT 3",
    )
    .bind(OWNER)
    .fetch_one(&pool)
    .await
    .unwrap();
    let root = &explained[0]["Plan"];
    let mut nodes = Vec::new();
    collect_plan_nodes(root, &mut nodes);
    assert!(
        nodes.iter().all(|node| !matches!(
            node.get("Node Type").and_then(Value::as_str),
            Some("Sort" | "Incremental Sort")
        )),
        "the retained owner index must satisfy stable ordering before LIMIT: {explained:#}"
    );
    let intended_child_indexes: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT child.relname
        FROM pg_class parent
        JOIN pg_namespace namespace ON namespace.oid = parent.relnamespace
        JOIN pg_inherits inheritance ON inheritance.inhparent = parent.oid
        JOIN pg_class child ON child.oid = inheritance.inhrelid
        WHERE namespace.nspname = current_schema()
          AND parent.relname = 'idx_retained_response_objects_owner_created'
        ORDER BY child.relname
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let intended_scans = nodes
        .iter()
        .filter(|node| {
            node.get("Index Name")
                .and_then(Value::as_str)
                .is_some_and(|name| intended_child_indexes.iter().any(|index| index == name))
        })
        .collect::<Vec<_>>();
    assert!(
        !intended_scans.is_empty(),
        "the default planner must use a child of the retained owner index: {explained:#}"
    );
    for scan in intended_scans {
        assert!(
            scan.get("Actual Rows").and_then(Value::as_f64).unwrap() <= 3.0,
            "each owner index scan must stop at the bounded seed limit: {explained:#}"
        );
        assert_eq!(
            scan.get("Actual Loops").and_then(Value::as_f64),
            Some(1.0),
            "each owner index scan must execute once: {explained:#}"
        );
    }
    assert_eq!(root["Node Type"], "Limit");
    assert_eq!(root["Actual Rows"].as_f64(), Some(3.0));
    assert_eq!(root["Actual Loops"].as_f64(), Some(1.0));
}

#[sqlx::test]
async fn erase_live_graph_preserves_a_template_shared_by_an_unrelated_request(pool: PgPool) {
    let erased = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "shared-template-erased",
    )
    .await;
    let survivor = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T11:00:00Z"),
        "shared-template-survivor",
    )
    .await;
    sqlx::query("UPDATE requests SET template_id = $1 WHERE id = $2")
        .bind(erased.template_ids[0])
        .bind(survivor.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM request_templates WHERE id = $1")
        .bind(survivor.template_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    assert_eq!(
        manager
            .delete_response_group(erased.group_id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(count_ids(&pool, "requests", &erased.request_ids).await, 0);
    assert_eq!(count_ids(&pool, "requests", &survivor.request_ids).await, 1);
    assert_eq!(
        count_ids(&pool, "request_templates", &erased.template_ids).await,
        1
    );
}

#[sqlx::test]
async fn owned_erasure_denies_a_mixed_owner_graph_without_mutation(pool: PgPool) {
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    sqlx::query("UPDATE requests SET created_by = 'different-owner' WHERE id = $1")
        .bind(graph.request_ids[1])
        .execute(&pool)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let error = manager
        .delete_owned_response_group(graph.group_id, OWNER)
        .await
        .expect_err("one differently-owned member must deny the whole graph");
    assert!(matches!(
        error,
        fusillade_arsenal::error::FusilladeError::RequestNotFound(_)
    ));
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 2);
    assert_eq!(
        count_ids(&pool, "response_steps", &graph.step_ids).await,
        graph.step_ids.len() as i64
    );
    assert_eq!(
        count_ids(&pool, "request_templates", &graph.template_ids).await,
        2
    );
    let fences: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_resurrection_fences WHERE object_id = ANY($1)",
    )
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fences, 0);

    assert_eq!(
        manager.delete_response_group(graph.group_id).await.unwrap(),
        2,
        "the explicit privileged erasure path must remain available"
    );
}

#[sqlx::test]
async fn owned_erasure_accepts_the_complete_owner_graph(pool: PgPool) {
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let manager = manager(&pool).await;

    assert_eq!(
        manager
            .delete_owned_response_group(graph.step_ids[1], OWNER)
            .await
            .unwrap(),
        2
    );
    assert_wholly_erased(&pool, &graph).await;
}

async fn assert_retained_erasure_rejects_corruption(
    pool: &PgPool,
    manager: &PostgresRequestManager<TestDbPools>,
    graph: &LiveGraph,
) {
    let error = manager
        .delete_response_group(graph.group_id)
        .await
        .expect_err("corrupt retained graph must fail closed");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_retained(pool, graph).await;
    let erased_fences: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_resurrection_fences \
         WHERE object_id = ANY($1) AND reason = 'erased'",
    )
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(erased_fences, 0);
}

#[sqlx::test]
async fn retained_erasure_validates_all_rows_routes_and_payload_topology(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let manager = manager(&pool).await;
    archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();
    let delete_on: NaiveDate = sqlx::query_scalar(
        "SELECT delete_on FROM retained_response_group_routes WHERE group_id = $1",
    )
    .bind(graph.group_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_request: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM retained_response_objects \
         WHERE delete_on = $1 AND group_id = $2 AND object_id = $3",
    )
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_step: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM retained_response_objects \
         WHERE delete_on = $1 AND group_id = $2 AND object_id = $3",
    )
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.step_ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();

    // A count-preserving object-kind substitution must not pass merely because
    // the header and route cardinalities still match.
    sqlx::query(
        "UPDATE retained_response_objects SET payload = $1 \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(&original_step)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
    sqlx::query(
        "UPDATE retained_response_objects SET payload = $1 \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(&original_request)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();

    // Row-vs-payload identities are checked for both request and step objects.
    sqlx::query(
        "UPDATE retained_response_objects \
         SET payload = jsonb_set(payload, '{request,id}', to_jsonb($1::text)) \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
    sqlx::query(
        "UPDATE retained_response_objects SET payload = $1 \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(&original_request)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE retained_response_objects \
         SET payload = jsonb_set(payload, '{step,id}', to_jsonb($1::text)) \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.step_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
    sqlx::query(
        "UPDATE retained_response_objects SET payload = $1 \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(&original_step)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.step_ids[0])
    .execute(&pool)
    .await
    .unwrap();

    // The request/template association and duplicated owner metadata are also
    // part of the immutable retained representation.
    sqlx::query(
        "UPDATE retained_response_objects \
         SET payload = jsonb_set(payload, '{request,template_id}', to_jsonb($1::text)) \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
    sqlx::query(
        "UPDATE retained_response_objects SET payload = $1 \
         WHERE delete_on = $2 AND group_id = $3 AND object_id = $4",
    )
    .bind(&original_request)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE retained_response_objects SET created_by = 'different-owner' \
         WHERE delete_on = $1 AND group_id = $2 AND object_id = $3",
    )
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
    sqlx::query(
        "UPDATE retained_response_objects SET created_by = $1, payload = $2 \
         WHERE delete_on = $3 AND group_id = $4 AND object_id = $5",
    )
    .bind(OWNER)
    .bind(&original_request)
    .bind(delete_on)
    .bind(graph.group_id)
    .bind(graph.request_ids[0])
    .execute(&pool)
    .await
    .unwrap();

    let owner_error = manager
        .delete_owned_response_group(graph.group_id, "different-owner")
        .await
        .expect_err("retained ownership mismatch must be a typed not-found");
    assert!(matches!(
        owner_error,
        fusillade_arsenal::error::FusilladeError::RequestNotFound(_)
    ));
    assert_wholly_retained(&pool, &graph).await;

    // Exact route membership is required even when every object remains.
    sqlx::query("DELETE FROM retained_response_request_routes WHERE request_id = $1")
        .bind(graph.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM retained_response_step_routes WHERE step_id = $1")
        .bind(graph.step_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_retained_erasure_rejects_corruption(&pool, &manager, &graph).await;
}

#[sqlx::test]
async fn erase_racing_same_id_create_rolls_back_request_and_template(pool: PgPool) {
    const ERASE_AFTER_DELETE_GATE: i64 = 730_018;

    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-create-race",
    )
    .await;
    sqlx::query(
        r#"
        CREATE FUNCTION test_gate_after_request_erasure()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            PERFORM pg_advisory_xact_lock(730018);
            RETURN NULL;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TRIGGER test_gate_after_request_erasure
        AFTER DELETE ON requests
        FOR EACH STATEMENT
        EXECUTE FUNCTION test_gate_after_request_erasure()
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut erase_gate = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ERASE_AFTER_DELETE_GATE)
        .execute(&mut *erase_gate)
        .await
        .unwrap();
    let erase_manager = manager(&pool).await;
    let erased_id = graph.request_ids[0];
    let mut eraser =
        tokio::spawn(async move { erase_manager.delete_response_group(erased_id).await });
    tokio::select! {
        result = &mut eraser => panic!("erasure completed before the after-delete gate: {result:?}"),
        () = wait_for_advisory_waiter_key(&pool, ERASE_AFTER_DELETE_GATE) => {}
    }

    let creator_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let creator_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&creator_pool)
        .await
        .unwrap();
    let creator_manager = PostgresRequestManager::new(
        WriteSignalingPools {
            read: creator_pool.clone(),
            write: creator_pool,
            write_requested: Arc::new(AtomicBool::new(false)),
        },
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600));
    let mut creator = tokio::spawn(async move {
        creator_manager
            .create_realtime(CreateRealtimeInput {
                request_id: erased_id,
                body: "must_not_survive".to_owned(),
                model: MODEL.to_owned(),
                endpoint: "https://example.invalid".to_owned(),
                method: "POST".to_owned(),
                path: "/v1/responses".to_owned(),
                api_key: "must-not-survive".to_owned(),
                created_by: OWNER.to_owned(),
            })
            .await
    });
    tokio::select! {
        result = &mut creator => panic!("same-ID creator did not block on erasure: {result:?}"),
        () = wait_for_backend_lock_waiter(&pool, creator_pid) => {}
    }

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(ERASE_AFTER_DELETE_GATE)
        .fetch_one(&mut *erase_gate)
        .await
        .unwrap();
    assert!(unlocked);
    assert_eq!(eraser.await.unwrap().unwrap(), 1);
    let error = creator
        .await
        .unwrap()
        .expect_err("same-ID create must roll back after the erasure commits");
    assert_write_error(&error, RetainedResponseWriteError::NotFound);
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 0);
    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates WHERE body = 'must_not_survive' OR api_key = 'must-not-survive'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leaked, 0);
}

fn late_realtime_record(request_id: Uuid) -> PersistCompletedRealtimeInput {
    PersistCompletedRealtimeInput {
        request_id,
        response_body: r#"{"must_not":"survive"}"#.to_owned(),
        status_code: 200,
        request_body: r#"{"must_not":"be_synthesized"}"#.to_owned(),
        model: MODEL.to_owned(),
        endpoint: "https://example.invalid".to_owned(),
        method: "POST".to_owned(),
        path: "/v1/responses".to_owned(),
        api_key: "must-not-survive".to_owned(),
        created_by: OWNER.to_owned(),
        started_at: Utc::now(),
        completed_at: Utc::now(),
    }
}

fn generic_terminal_write(graph: &LiveGraph) -> Request<Completed> {
    let now = Utc::now();
    Request {
        data: RequestData {
            id: RequestId(graph.request_ids[0]),
            batch_id: None,
            template_id: TemplateId(graph.template_ids[0]),
            custom_id: None,
            endpoint: "https://example.invalid".to_owned(),
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            body: r#"{"must_not":"survive"}"#.to_owned(),
            model: MODEL.to_owned(),
            api_key: "must-not-survive".to_owned(),
            created_by: OWNER.to_owned(),
            batch_metadata: HashMap::new(),
        },
        state: Completed {
            response_status: 200,
            response_body: r#"{"must_not":"survive"}"#.to_owned(),
            claimed_at: now,
            started_at: now,
            completed_at: now,
            routed_model: MODEL.to_owned(),
        },
    }
}

#[sqlx::test]
async fn bulk_duplicate_identical_id_persists_one_request_and_one_template(pool: PgPool) {
    let manager = manager(&pool).await;
    let request_id = Uuid::new_v4();
    let record = late_realtime_record(request_id);

    manager
        .persist_completed_realtime_batch(&[record.clone(), record])
        .await
        .expect("an identical duplicate delivery is one idempotent write");

    let rows: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT id, template_id FROM requests WHERE id = $1")
            .bind(request_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    let template_id = rows[0]
        .1
        .expect("the synthesized request must reference a template");
    let templates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates \
         WHERE id = $1 OR body LIKE '%must_not%_synthesized%'",
    )
    .bind(template_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        templates, 1,
        "no payload-bearing duplicate template may remain"
    );
}

#[sqlx::test]
async fn bulk_duplicate_conflicting_id_uses_first_record_without_orphans(pool: PgPool) {
    let manager = manager(&pool).await;
    let request_id = Uuid::new_v4();
    let mut first = late_realtime_record(request_id);
    first.request_body = r#"{"winner":"request"}"#.to_owned();
    first.response_body = r#"{"winner":"response"}"#.to_owned();
    first.model = "winner-model".to_owned();
    first.api_key = "winner-key".to_owned();
    first.created_by = "winner-owner".to_owned();
    let mut second = first.clone();
    second.request_body = r#"{"loser":"request"}"#.to_owned();
    second.response_body = r#"{"loser":"response"}"#.to_owned();
    second.model = "loser-model".to_owned();
    second.api_key = "loser-key".to_owned();
    second.created_by = "loser-owner".to_owned();

    manager
        .persist_completed_realtime_batch(&[first, second])
        .await
        .expect("the first occurrence defines a duplicate identity");

    let row: (String, String, String, String, String) = sqlx::query_as(
        r#"
        SELECT request.response_body, request.model, request.created_by,
               template.body, template.api_key
        FROM requests request
        JOIN request_templates template ON template.id = request.template_id
        WHERE request.id = $1
        "#,
    )
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            r#"{"winner":"response"}"#.to_owned(),
            "winner-model".to_owned(),
            "winner-owner".to_owned(),
            r#"{"winner":"request"}"#.to_owned(),
            "winner-key".to_owned(),
        )
    );
    let templates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates WHERE body LIKE '%winner%' OR body LIKE '%loser%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        templates, 1,
        "the losing duplicate must not allocate a template"
    );
}

#[sqlx::test]
async fn bulk_mixed_fenced_batch_rolls_back_live_and_fresh_siblings(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let retained = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "bulk-mixed-retained",
    )
    .await;
    let erased = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "bulk-mixed-erased",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 2, i64::MAX)
        .await
        .unwrap();
    manager
        .delete_response_group(erased.request_ids[0])
        .await
        .unwrap();

    let live_id = Uuid::new_v4();
    manager
        .create_realtime(CreateRealtimeInput {
            request_id: live_id,
            body: "bulk-live-body".to_owned(),
            model: MODEL.to_owned(),
            endpoint: "https://example.invalid".to_owned(),
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            api_key: "bulk-live-key".to_owned(),
            created_by: OWNER.to_owned(),
        })
        .await
        .unwrap();
    let fresh_id = Uuid::new_v4();

    let error = manager
        .persist_completed_realtime_batch(&[
            late_realtime_record(retained.request_ids[0]),
            late_realtime_record(erased.request_ids[0]),
            late_realtime_record(live_id),
            late_realtime_record(fresh_id),
        ])
        .await
        .expect_err("one fenced identity must fail the mixed transaction atomically");
    assert_write_error(&error, RetainedResponseWriteError::NotFound);

    let live_state: String = sqlx::query_scalar("SELECT state FROM requests WHERE id = $1")
        .bind(live_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live_state, "processing");
    assert_eq!(count_ids(&pool, "requests", &[fresh_id]).await, 0);
    assert_wholly_retained(&pool, &retained).await;
    assert_wholly_erased(&pool, &erased).await;
    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates WHERE body LIKE '%must_not%_synthesized%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leaked, 0);
}

fn assert_write_error(
    error: &fusillade_arsenal::error::FusilladeError,
    expected: RetainedResponseWriteError,
) {
    assert_eq!(
        RetainedResponseWriteError::from_fusillade_error(error),
        Some(expected)
    );
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("must_not"));
}

#[sqlx::test]
async fn late_request_and_step_writers_cannot_resurrect_an_erased_graph(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let manager = manager(&pool).await;
    archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();
    manager.delete_response_group(graph.group_id).await.unwrap();

    for error in [
        manager
            .complete_request(
                RequestId(graph.request_ids[0]),
                r#"{"must_not":"survive"}"#,
                200,
            )
            .await
            .expect_err("a late completion must be fenced"),
        manager
            .fail_request(RequestId(graph.request_ids[0]), "must_not_survive", 500)
            .await
            .expect_err("a late failure must be fenced"),
        manager
            .create_realtime(CreateRealtimeInput {
                request_id: graph.request_ids[0],
                body: r#"{"must_not":"survive"}"#.to_owned(),
                model: MODEL.to_owned(),
                endpoint: "https://example.invalid".to_owned(),
                method: "POST".to_owned(),
                path: "/v1/responses".to_owned(),
                api_key: "must-not-survive".to_owned(),
                created_by: OWNER.to_owned(),
            })
            .await
            .expect_err("a late create must be fenced"),
    ] {
        assert_write_error(&error, RetainedResponseWriteError::NotFound);
    }
    let bulk_error = manager
        .persist_completed_realtime_batch(&[late_realtime_record(graph.request_ids[0])])
        .await
        .expect_err("bulk late persistence must report an erased identity");
    assert_write_error(&bulk_error, RetainedResponseWriteError::NotFound);

    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let step_manager = PostgresResponseStepManager::new(pools);
    let create_error = step_manager
        .create_step(CreateStepInput {
            id: Some(graph.step_ids[0]),
            request_id: None,
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ToolCall,
            step_sequence: 100,
            request_payload: json!({"must_not": "survive"}),
        })
        .await
        .expect_err("an erased step must not be recreated");
    assert!(matches!(
        &create_error,
        fusillade_arsenal::error::FusilladeError::Other(source)
            if source.downcast_ref::<ResponseStepNotFound>().is_some()
    ));
    step_manager
        .complete_step(StepId(graph.step_ids[0]), json!({"must_not": "survive"}))
        .await
        .expect_err("an erased step must remain absent");

    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn late_request_writers_treat_an_active_retained_graph_as_terminal(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "late-active-retained",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();

    for error in [
        manager
            .complete_request(RequestId(graph.request_ids[0]), "must_not_survive", 200)
            .await
            .expect_err("a retained completion must be terminal"),
        manager
            .fail_request(RequestId(graph.request_ids[0]), "must_not_survive", 500)
            .await
            .expect_err("a retained failure must be terminal"),
        manager
            .create_realtime(CreateRealtimeInput {
                request_id: graph.request_ids[0],
                body: "must_not_survive".to_owned(),
                model: MODEL.to_owned(),
                endpoint: "https://example.invalid".to_owned(),
                method: "POST".to_owned(),
                path: "/v1/responses".to_owned(),
                api_key: "must-not-survive".to_owned(),
                created_by: OWNER.to_owned(),
            })
            .await
            .expect_err("a retained request must not be recreated"),
    ] {
        assert_write_error(&error, RetainedResponseWriteError::AlreadyRetained);
    }
    manager
        .persist_completed_realtime_batch(&[late_realtime_record(graph.request_ids[0])])
        .await
        .expect("bulk replay is an idempotent terminal outcome");

    assert_wholly_retained(&pool, &graph).await;
    let orphan_templates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates WHERE body LIKE '%must_not%' OR api_key = 'must-not-survive'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphan_templates, 0);
}

#[sqlx::test]
async fn generic_persist_and_retry_paths_share_retained_and_fenced_outcomes(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Failed,
        timestamp("2026-08-01T10:00:00Z"),
        "generic-writers",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();
    let terminal = generic_terminal_write(&graph);

    assert_eq!(manager.persist(&terminal).await.unwrap(), None);
    assert!(
        !manager
            .reschedule_for_retry(
                RequestId(graph.request_ids[0]),
                DaemonId(Uuid::new_v4()),
                3,
                None,
            )
            .await
            .unwrap()
    );
    let retained_retry = manager
        .retry_failed_requests(vec![RequestId(graph.request_ids[0])])
        .await
        .expect_err("manual retry must not thaw immutable retained content");
    assert_write_error(&retained_retry, RetainedResponseWriteError::AlreadyRetained);
    assert_wholly_retained(&pool, &graph).await;

    manager.delete_response_group(graph.group_id).await.unwrap();
    for error in [
        manager
            .persist(&terminal)
            .await
            .expect_err("generic persist must honor the erasure fence"),
        manager
            .reschedule_for_retry(
                RequestId(graph.request_ids[0]),
                DaemonId(Uuid::new_v4()),
                4,
                None,
            )
            .await
            .expect_err("daemon retry must honor the erasure fence"),
        manager
            .retry_failed_requests(vec![RequestId(graph.request_ids[0])])
            .await
            .expect_err("manual retry must honor the erasure fence"),
    ] {
        assert_write_error(&error, RetainedResponseWriteError::NotFound);
    }
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn generic_persist_waits_for_erasure_then_observes_the_fence(pool: PgPool) {
    const ERASE_GATE: i64 = 730_019;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Pending,
        timestamp("2026-08-01T10:00:00Z"),
        "generic-erase-race",
    )
    .await;
    sqlx::query(
        r#"
        CREATE FUNCTION gate_generic_persist_erasure() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            PERFORM pg_advisory_xact_lock(730019);
            RETURN NULL;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER gate_generic_persist_erasure AFTER DELETE ON requests \
         FOR EACH STATEMENT EXECUTE FUNCTION gate_generic_persist_erasure()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut gate = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(ERASE_GATE)
        .execute(&mut *gate)
        .await
        .unwrap();
    let erase_manager = manager(&pool).await;
    let erase_id = graph.group_id;
    let mut eraser =
        tokio::spawn(async move { erase_manager.delete_response_group(erase_id).await });
    tokio::select! {
        result = &mut eraser => panic!("erasure completed before the delete gate: {result:?}"),
        () = wait_for_advisory_waiter_key(&pool, ERASE_GATE) => {}
    }

    let persist_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let persist_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&persist_pool)
        .await
        .unwrap();
    let persist_manager = PostgresRequestManager::new(
        TestDbPools::new(persist_pool).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600));
    let terminal = generic_terminal_write(&graph);
    let mut persister = tokio::spawn(async move { persist_manager.persist(&terminal).await });
    tokio::select! {
        result = &mut persister => panic!("generic persist did not wait for the graph lifecycle lock: {result:?}"),
        () = wait_for_backend_lock_waiter(&pool, persist_pid) => {}
    }

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(ERASE_GATE)
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    assert!(unlocked);
    assert_eq!(eraser.await.unwrap().unwrap(), 1);
    let error = persister
        .await
        .unwrap()
        .expect_err("persist must observe the committed erasure fence");
    assert_write_error(&error, RetainedResponseWriteError::NotFound);
    assert_wholly_erased(&pool, &graph).await;
}

#[sqlx::test]
async fn late_synthesis_is_fenced_after_the_retained_partition_disappears(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "late-after-drop",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();

    sqlx::query("ALTER TABLE retained_response_objects DETACH PARTITION retained_response_objects_d20260902")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE retained_response_objects_d20260902")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE retained_response_buckets SET state = 'retired' WHERE delete_on = $1")
        .bind(delete_on)
        .execute(&pool)
        .await
        .unwrap();

    let bulk_error = manager
        .persist_completed_realtime_batch(&[late_realtime_record(graph.request_ids[0])])
        .await
        .expect_err("retired bulk persistence must report an unavailable identity");
    assert_write_error(&bulk_error, RetainedResponseWriteError::NotFound);
    let error = manager
        .create_realtime(CreateRealtimeInput {
            request_id: graph.request_ids[0],
            body: r#"{"must_not":"survive"}"#.to_owned(),
            model: MODEL.to_owned(),
            endpoint: "https://example.invalid".to_owned(),
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            api_key: "must-not-survive".to_owned(),
            created_by: OWNER.to_owned(),
        })
        .await
        .expect_err("retired IDs must not synthesize new content");
    assert_write_error(&error, RetainedResponseWriteError::NotFound);
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 0);
    let orphan_templates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_templates WHERE body LIKE '%must_not%' OR api_key = 'must-not-survive'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphan_templates, 0);
}

#[sqlx::test]
async fn retiring_route_blocks_late_synthesis_after_archive_fence_expiry(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "late-after-fence-expiry",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("graph movement must succeed");
    sqlx::query(
        "UPDATE retained_response_resurrection_fences \
         SET reason = 'archived', expires_at = NOW() - INTERVAL '1 second' \
         WHERE object_id = ANY($1)",
    )
    .bind(
        std::iter::once(graph.group_id)
            .chain(graph.request_ids.iter().copied())
            .chain(graph.step_ids.iter().copied())
            .collect::<Vec<_>>(),
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut retiring = pool.begin().await.unwrap();
    fence_partition_for_retirement(&mut retiring, delete_on).await;
    retiring.commit().await.unwrap();

    let error = manager
        .create_realtime(CreateRealtimeInput {
            request_id: graph.request_ids[0],
            body: r#"{"must_not":"survive"}"#.to_owned(),
            model: MODEL.to_owned(),
            endpoint: "https://example.invalid".to_owned(),
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            api_key: "must-not-survive".to_owned(),
            created_by: OWNER.to_owned(),
        })
        .await
        .expect_err("a retiring route is an unconditional resurrection fence");
    assert_write_error(&error, RetainedResponseWriteError::NotFound);
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM request_templates \
             WHERE body LIKE '%must_not%' OR api_key = 'must-not-survive'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    for owner in [OWNER, "different-owner"] {
        let error = manager
            .delete_owned_response_group(graph.request_ids[0], owner)
            .await
            .expect_err("retirement must not become an ownership oracle");
        assert!(matches!(
            error,
            fusillade_arsenal::error::FusilladeError::RequestNotFound(_)
        ));
    }
}

#[sqlx::test]
async fn explicit_erasure_of_an_already_retired_route_is_idempotently_unavailable(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-after-retirement",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("graph movement must succeed");
    let mut retirement = pool.begin().await.unwrap();
    fence_partition_for_retirement(&mut retirement, delete_on).await;
    retirement.commit().await.unwrap();
    sqlx::query(
        "ALTER TABLE retained_response_objects \
         DETACH PARTITION retained_response_objects_d20260902",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DROP TABLE retained_response_objects_d20260902")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        WITH completion AS (SELECT clock_timestamp() AS completed_at),
        bucket_update AS (
            UPDATE retained_response_buckets
            SET state = 'retired', state_changed_at = completion.completed_at
            FROM completion
            WHERE delete_on = $1
            RETURNING state_changed_at
        )
        UPDATE retention_partition_retirements journal
        SET completed_at = bucket_update.state_changed_at
        FROM bucket_update
        WHERE journal.parent_table = 'retained_response_objects'
          AND journal.lower_bound = $1
        "#,
    )
    .bind(delete_on)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        manager
            .delete_response_group(graph.request_ids[0])
            .await
            .expect("retired content is already unavailable"),
        0
    );
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 0);
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 0);
}

#[sqlx::test]
async fn explicit_erasure_of_a_retiring_route_never_races_partition_retirement(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-during-retirement",
    )
    .await;
    let manager = manager(&pool).await;
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("graph movement must succeed");
    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(delete_on)
        .execute(&pool)
        .await
        .unwrap();

    let error = manager
        .delete_response_group(graph.request_ids[0])
        .await
        .expect_err("an orphan retiring fence cannot certify deletion");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::RetirementIdentityMismatch)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM retained_response_objects \
             WHERE delete_on = $1 AND group_id = $2",
        )
        .bind(delete_on)
        .bind(graph.group_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        2,
        "explicit erasure must not inspect or mutate a retiring partition",
    );
}

#[sqlx::test]
async fn retirement_fence_winning_the_partition_lock_prevents_payload_erasure(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "erase-partition-lock-race",
    )
    .await;
    let manager = Arc::new(manager(&pool).await);
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();

    let mut retirement = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
             hashtextextended(\
                 'retained_response_objects.partition:' || current_schema() || ':'\
                     || to_char($1::date, 'YYYYMMDD'),\
                 0\
             )\
         )",
    )
    .bind(delete_on)
    .execute(&mut *retirement)
    .await
    .unwrap();
    let request_id = graph.request_ids[0];
    let eraser = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.delete_response_group(request_id).await })
    };
    wait_for_advisory_waiter(&pool).await;
    fence_partition_for_retirement(&mut retirement, delete_on).await;
    retirement.commit().await.unwrap();

    assert_eq!(eraser.await.unwrap().unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM retained_response_objects \
             WHERE delete_on = $1 AND group_id = $2",
        )
        .bind(delete_on)
        .bind(graph.group_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        2,
    );
}

#[sqlx::test]
async fn creator_erasure_retries_a_partition_retirement_race(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "creator-partition-lock-race",
    )
    .await;
    let manager = Arc::new(manager(&pool).await);
    archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();

    let mut retirement = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
             hashtextextended(\
                 'retained_response_objects.partition:' || current_schema() || ':'\
                     || to_char($1::date, 'YYYYMMDD'),\
                 0\
             )\
         )",
    )
    .bind(delete_on)
    .execute(&mut *retirement)
    .await
    .unwrap();
    let eraser = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.bulk_delete_data(OWNER, 10).await })
    };
    wait_for_advisory_waiter(&pool).await;
    fence_partition_for_retirement(&mut retirement, delete_on).await;
    retirement.commit().await.unwrap();

    let error = eraser
        .await
        .unwrap()
        .expect_err("creator erasure must retry until physical retirement is proven");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::RetirementPending)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM retained_response_objects \
             WHERE delete_on = $1 AND group_id = $2",
        )
        .bind(delete_on)
        .bind(graph.group_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        2,
    );
}

async fn manager(pool: &PgPool) -> PostgresRequestManager<TestDbPools> {
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone())
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600))
}

async fn managers(
    pool: &PgPool,
) -> (
    PostgresRequestManager<TestDbPools>,
    PostgresResponseStepManager<TestDbPools>,
) {
    let pools = TestDbPools::new(pool.clone())
        .await
        .expect("test pools must initialize");
    (
        PostgresRequestManager::new(pools.clone(), PostgresStorageConfig::default()),
        PostgresResponseStepManager::new(pools),
    )
}

async fn write_gated_manager(
    pool: &PgPool,
) -> (
    PostgresRequestManager<WriteSignalingPools>,
    PoolConnection<Postgres>,
    Arc<AtomicBool>,
) {
    let write = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("gated write pool must connect");
    let held_connection = write
        .acquire()
        .await
        .expect("the sole write connection must be held");
    let write_requested = Arc::new(AtomicBool::new(false));
    let manager = PostgresRequestManager::new(
        WriteSignalingPools {
            read: pool.clone(),
            write,
            write_requested: Arc::clone(&write_requested),
        },
        PostgresStorageConfig::default(),
    );
    (manager, held_connection, write_requested)
}

async fn discovery_read_gated_manager(
    pool: &PgPool,
) -> (
    PostgresRequestManager<DiscoveryReadGatedPools>,
    PoolConnection<Postgres>,
    Arc<AtomicBool>,
) {
    let gated_read = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("gated read pool must connect");
    let held_connection = gated_read
        .acquire()
        .await
        .expect("the sole gated read connection must be held");
    let gate_requested = Arc::new(AtomicBool::new(false));
    let manager = PostgresRequestManager::new(
        DiscoveryReadGatedPools {
            read: pool.clone(),
            gated_read,
            read_selections: Arc::new(AtomicUsize::new(0)),
            gate_requested: Arc::clone(&gate_requested),
        },
        PostgresStorageConfig::default(),
    );
    (manager, held_connection, gate_requested)
}

async fn wait_for_write_request(write_requested: &AtomicBool) {
    while !write_requested.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_advisory_waiter(pool: &PgPool) {
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted)",
        )
        .fetch_one(pool)
        .await
        .expect("advisory lock wait must be observable");
        if waiting {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_advisory_waiter_key(pool: &PgPool, key: i64) {
    loop {
        let waiting: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_locks
                WHERE locktype = 'advisory'
                  AND classid = 0
                  AND objid = $1::oid
                  AND NOT granted
            )
            "#,
        )
        .bind(key as i32)
        .fetch_one(pool)
        .await
        .expect("the keyed advisory-lock wait must be observable");
        if waiting {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_relation_waiter(pool: &PgPool, relation: &str) {
    loop {
        let waiting: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_locks
                WHERE locktype = 'relation'
                  AND relation = to_regclass($1)
                  AND mode = 'AccessShareLock'
                  AND NOT granted
            )
            "#,
        )
        .bind(relation)
        .fetch_one(pool)
        .await
        .expect("the retained-route relation wait must be observable");
        if waiting {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_backend_lock_waiter(pool: &PgPool, backend_pid: i32) {
    loop {
        let waiting: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_locks
                WHERE pid = $1
                  AND NOT granted
            )
            "#,
        )
        .bind(backend_pid)
        .fetch_one(pool)
        .await
        .expect("backend lock wait must be observable");
        if waiting {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_backend_blocked_by(pool: &PgPool, waiter_pid: i32, blocker_pid: i32) {
    loop {
        let blocked: bool = sqlx::query_scalar("SELECT $2::int4 = ANY(pg_blocking_pids($1))")
            .bind(waiter_pid)
            .bind(blocker_pid)
            .fetch_one(pool)
            .await
            .expect("the graph lock blocker must be observable");
        if blocked {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn lock_session_response_graph(connection: &mut PoolConnection<Postgres>, group_id: Uuid) {
    sqlx::query(
        r#"
        SELECT pg_advisory_lock(
            hashtextextended(
                'retained_response_graph:' || current_schema() || ':' || $1::text,
                0
            )
        )
        "#,
    )
    .bind(group_id)
    .execute(&mut **connection)
    .await
    .expect("the graph lock must be acquired");
}

async fn unlock_session_response_graph(connection: &mut PoolConnection<Postgres>, group_id: Uuid) {
    let unlocked: bool = sqlx::query_scalar(
        r#"
        SELECT pg_advisory_unlock(
            hashtextextended(
                'retained_response_graph:' || current_schema() || ':' || $1::text,
                0
            )
        )
        "#,
    )
    .bind(group_id)
    .fetch_one(&mut **connection)
    .await
    .expect("the graph lock must be released");
    assert!(unlocked);
}

async fn install_candidate_index(pool: &PgPool) {
    sqlx::query(
        r#"
        CREATE INDEX idx_requests_batchless_retention_due
        ON requests (
          service_tier,
          (CASE state WHEN 'completed' THEN completed_at
                      WHEN 'failed' THEN failed_at
                      WHEN 'canceled' THEN canceled_at END),
          id
        )
        WHERE batch_id IS NULL
          AND state IN ('completed', 'failed', 'canceled')
        "#,
    )
    .execute(pool)
    .await
    .expect("candidate index must install");
    let ready: bool = sqlx::query_scalar("SELECT retained_response_archive_index_ready(NULL)")
        .fetch_one(pool)
        .await
        .expect("candidate readiness must be queryable");
    assert!(ready, "test candidate index must satisfy the runtime guard");
}

async fn ensure_partition(pool: &PgPool, delete_on: NaiveDate) {
    sqlx::query("SELECT ensure_retained_response_partition($1, NULL)")
        .bind(delete_on)
        .execute(pool)
        .await
        .expect("retained response partition must be available");
}

async fn fence_partition_for_retirement(tx: &mut Transaction<'_, Postgres>, delete_on: NaiveDate) {
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
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE retained_response_buckets \
         SET state = 'retiring', state_changed_at = statement_timestamp() \
         WHERE delete_on = $1 AND state = 'active'",
    )
    .bind(delete_on)
    .execute(&mut **tx)
    .await
    .unwrap();
}

async fn insert_request(
    pool: &PgPool,
    tier: &str,
    state: TerminalState,
    terminal_at: DateTime<Utc>,
    body_suffix: &str,
) -> (Uuid, Uuid) {
    let request_id = Uuid::new_v4();
    let template_id = Uuid::new_v4();
    let body = format!(r#"{{"prompt":"fixture-{body_suffix}"}}"#);
    sqlx::query(
        r#"
        INSERT INTO request_templates (
            id, file_id, custom_id, endpoint, method, path, body, model,
            api_key, line_number, body_byte_size, metadata, created_at, updated_at
        ) VALUES (
            $1, NULL, $2, 'http://retention.invalid', 'POST', '/v1/responses',
            $3, $4, 'secret-test-key', 7, $5, $6,
            $7 - INTERVAL '1 hour', $7 - INTERVAL '30 minutes'
        )
        "#,
    )
    .bind(template_id)
    .bind(format!("custom-{body_suffix}"))
    .bind(&body)
    .bind(MODEL)
    .bind(i64::try_from(body.len()).unwrap())
    .bind(json!({"user_agent": "retention-test"}))
    .bind(terminal_at)
    .execute(pool)
    .await
    .expect("dedicated template must insert");

    let (
        state_name,
        claimed_at,
        started_at,
        response_status,
        response_body,
        completed_at,
        error,
        failed_at,
        canceled_at,
        routed_model,
    ) = match state {
        TerminalState::Completed => (
            "completed",
            Some(terminal_at - TimeDelta::minutes(2)),
            Some(terminal_at - TimeDelta::minutes(1)),
            Some(200_i16),
            Some(format!(r#"{{"answer":"{body_suffix}"}}"#)),
            Some(terminal_at),
            None,
            None,
            None,
            Some(MODEL.to_owned()),
        ),
        TerminalState::Failed => (
            "failed",
            Some(terminal_at - TimeDelta::minutes(2)),
            Some(terminal_at - TimeDelta::minutes(1)),
            None,
            None,
            None,
            Some(format!("fixture-error-{body_suffix}")),
            Some(terminal_at),
            None,
            Some(MODEL.to_owned()),
        ),
        TerminalState::Canceled { dispatched } => (
            "canceled",
            dispatched.then_some(terminal_at - TimeDelta::minutes(2)),
            dispatched.then_some(terminal_at - TimeDelta::minutes(1)),
            None,
            None,
            None,
            None,
            None,
            Some(terminal_at),
            None,
        ),
        TerminalState::Pending => (
            "pending", None, None, None, None, None, None, None, None, None,
        ),
    };

    sqlx::query(
        r#"
        INSERT INTO requests (
            id, batch_id, template_id, model, custom_id, state, retry_attempt,
            not_before, daemon_id, claimed_at, started_at, response_status,
            response_body, completed_at, error, failed_at, canceled_at,
            response_size, routed_model, service_tier, created_by,
            created_at, updated_at
        ) VALUES (
            $1, NULL, $2, $3, $4, $5, 2,
            NULL, NULL, $6, $7, $8,
            $9, $10, $11, $12, $13,
            $14, $15, $16, $17,
            $18 - INTERVAL '2 hours', $18
        )
        "#,
    )
    .bind(request_id)
    .bind(template_id)
    .bind(MODEL)
    .bind(format!("custom-{body_suffix}"))
    .bind(state_name)
    .bind(claimed_at)
    .bind(started_at)
    .bind(response_status)
    .bind(&response_body)
    .bind(completed_at)
    .bind(&error)
    .bind(failed_at)
    .bind(canceled_at)
    .bind(response_body.as_ref().map_or(0, |body| body.len()) as i64)
    .bind(routed_model)
    .bind(tier)
    .bind(OWNER)
    .bind(terminal_at)
    .execute(pool)
    .await
    .expect("batchless request must insert");
    (request_id, template_id)
}

async fn insert_step(pool: &PgPool, step: StepFixture) {
    let StepFixture {
        id,
        request_id,
        prev_step_id,
        parent_step_id,
        sequence,
        state,
        terminal_at,
    } = step;
    let (
        kind,
        state_name,
        started_at,
        response_payload,
        completed_at,
        error,
        failed_at,
        canceled_at,
    ) = match state {
        TerminalState::Completed => (
            if request_id.is_some() {
                "model_call"
            } else {
                "tool_call"
            },
            "completed",
            Some(terminal_at - TimeDelta::minutes(1)),
            Some(json!({"sequence": sequence, "result": "done"})),
            Some(terminal_at),
            None,
            None,
            None,
        ),
        TerminalState::Failed => (
            if request_id.is_some() {
                "model_call"
            } else {
                "tool_call"
            },
            "failed",
            Some(terminal_at - TimeDelta::minutes(1)),
            None,
            None,
            Some(json!({"sequence": sequence, "error": "failed"})),
            Some(terminal_at),
            None,
        ),
        TerminalState::Canceled { .. } => (
            if request_id.is_some() {
                "model_call"
            } else {
                "tool_call"
            },
            "canceled",
            Some(terminal_at - TimeDelta::minutes(1)),
            None,
            None,
            None,
            None,
            Some(terminal_at),
        ),
        TerminalState::Pending => (
            if request_id.is_some() {
                "model_call"
            } else {
                "tool_call"
            },
            "pending",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    };
    sqlx::query(
        r#"
        INSERT INTO response_steps (
            id, request_id, prev_step_id, parent_step_id, step_kind,
            step_sequence, request_payload, response_payload, state,
            started_at, completed_at, failed_at, canceled_at, retry_attempt,
            error, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9,
            $10, $11, $12, $13, 1,
            $14, $15 - INTERVAL '2 hours', $15
        )
        "#,
    )
    .bind(id)
    .bind(request_id)
    .bind(prev_step_id)
    .bind(parent_step_id)
    .bind(kind)
    .bind(sequence)
    .bind(json!({"sequence": sequence, "input": "fixture"}))
    .bind(response_payload)
    .bind(state_name)
    .bind(started_at)
    .bind(completed_at)
    .bind(failed_at)
    .bind(canceled_at)
    .bind(error)
    .bind(terminal_at)
    .execute(pool)
    .await
    .expect("response step must insert");
}

async fn singleton(
    pool: &PgPool,
    tier: &str,
    state: TerminalState,
    terminal_at: DateTime<Utc>,
    suffix: &str,
) -> LiveGraph {
    let (request_id, template_id) = insert_request(pool, tier, state, terminal_at, suffix).await;
    LiveGraph {
        group_id: request_id,
        request_ids: vec![request_id],
        step_ids: Vec::new(),
        template_ids: vec![template_id],
    }
}

async fn branching_graph(pool: &PgPool, second_request_state: TerminalState) -> LiveGraph {
    branching_graph_with_tail_states(pool, second_request_state, second_request_state).await
}

async fn branching_graph_with_tail_states(
    pool: &PgPool,
    second_request_state: TerminalState,
    tail_step_state: TerminalState,
) -> LiveGraph {
    let first_terminal = timestamp("2026-08-01T10:00:00Z");
    let second_terminal = timestamp("2026-08-02T11:00:00Z");
    let (first_request, first_template) = insert_request(
        pool,
        "flex",
        TerminalState::Completed,
        first_terminal,
        "branch-head",
    )
    .await;
    let (second_request, second_template) = insert_request(
        pool,
        "priority",
        second_request_state,
        second_terminal,
        "branch-tail",
    )
    .await;
    let head = Uuid::new_v4();
    let parallel_a = Uuid::new_v4();
    let parallel_b = Uuid::new_v4();
    let nested_tool = Uuid::new_v4();
    let tail_model = Uuid::new_v4();

    insert_step(
        pool,
        StepFixture {
            id: head,
            request_id: Some(first_request),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: first_terminal,
        },
    )
    .await;
    insert_step(
        pool,
        StepFixture {
            id: parallel_a,
            request_id: None,
            prev_step_id: Some(head),
            parent_step_id: Some(head),
            sequence: 2,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-02T12:00:00Z"),
        },
    )
    .await;
    insert_step(
        pool,
        StepFixture {
            id: parallel_b,
            request_id: None,
            prev_step_id: Some(head),
            parent_step_id: Some(head),
            sequence: 3,
            state: TerminalState::Failed,
            terminal_at: timestamp("2026-08-02T13:00:00Z"),
        },
    )
    .await;
    insert_step(
        pool,
        StepFixture {
            id: nested_tool,
            request_id: None,
            prev_step_id: Some(parallel_a),
            parent_step_id: Some(head),
            sequence: 4,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-03T14:00:00Z"),
        },
    )
    .await;
    insert_step(
        pool,
        StepFixture {
            id: tail_model,
            request_id: Some(second_request),
            prev_step_id: Some(nested_tool),
            parent_step_id: Some(head),
            sequence: 5,
            state: tail_step_state,
            terminal_at: second_terminal,
        },
    )
    .await;

    let mut request_ids = vec![first_request, second_request];
    request_ids.sort_unstable();
    let mut step_ids = vec![head, parallel_a, parallel_b, nested_tool, tail_model];
    step_ids.sort_unstable();
    let mut template_ids = vec![first_template, second_template];
    template_ids.sort_unstable();
    LiveGraph {
        group_id: head,
        request_ids,
        step_ids,
        template_ids,
    }
}

async fn count_ids(pool: &PgPool, table: &str, ids: &[Uuid]) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE id = ANY($1)");
    sqlx::query_scalar(&sql)
        .bind(ids)
        .fetch_one(pool)
        .await
        .expect("live count must succeed")
}

async fn retained_counts(pool: &PgPool, group_id: Uuid) -> (i64, i64, i64) {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) FILTER (WHERE object_kind = 'group')::BIGINT AS groups,
               COUNT(*) FILTER (WHERE object_kind = 'request')::BIGINT AS requests,
               COUNT(*) FILTER (WHERE object_kind = 'step')::BIGINT AS steps
        FROM retained_response_objects
        WHERE group_id = $1
        "#,
    )
    .bind(group_id)
    .fetch_one(pool)
    .await
    .expect("retained counts must succeed");
    (
        row.get::<i64, _>("groups"),
        row.get::<i64, _>("requests"),
        row.get::<i64, _>("steps"),
    )
}

async fn distinct_delete_on_count(pool: &PgPool, group_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(DISTINCT delete_on)::BIGINT FROM retained_response_objects WHERE group_id = $1",
    )
    .bind(group_id)
    .fetch_one(pool)
    .await
    .expect("delete_on count must succeed")
}

async fn assert_wholly_live(pool: &PgPool, graph: &LiveGraph) {
    assert_eq!(
        count_ids(pool, "requests", &graph.request_ids).await,
        graph.request_ids.len() as i64
    );
    assert_eq!(
        count_ids(pool, "response_steps", &graph.step_ids).await,
        graph.step_ids.len() as i64
    );
    assert_eq!(
        count_ids(pool, "request_templates", &graph.template_ids).await,
        graph.template_ids.len() as i64
    );
    assert_eq!(retained_counts(pool, graph.group_id).await, (0, 0, 0));
}

async fn assert_wholly_retained(pool: &PgPool, graph: &LiveGraph) {
    assert_eq!(count_ids(pool, "requests", &graph.request_ids).await, 0);
    assert_eq!(count_ids(pool, "response_steps", &graph.step_ids).await, 0);
    assert_eq!(
        count_ids(pool, "request_templates", &graph.template_ids).await,
        0
    );
    assert_eq!(
        retained_counts(pool, graph.group_id).await,
        (
            1,
            graph.request_ids.len() as i64,
            graph.step_ids.len() as i64,
        )
    );
    assert_eq!(distinct_delete_on_count(pool, graph.group_id).await, 1);
}

async fn archive<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionPolicy,
    max_groups: i64,
    max_bytes: i64,
) -> fusillade_arsenal::error::Result<RetainedResponseArchiveOutcome> {
    // Pinned cutoffs pair with the suite's pinned fixture dates: both sides
    // of every movement comparison live in fixture time, so the real date
    // never leaks in. Tests whose fixtures use the real clock (the trailing
    // window assertions read SQL now()) must call `archive_at` with real-now
    // cutoffs instead of this helper.
    archive_at(
        manager,
        policy,
        timestamp("2026-08-31T00:00:00Z"),
        timestamp("2026-08-31T00:00:00Z"),
        max_groups,
        max_bytes,
    )
    .await
}

async fn archive_at<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionPolicy,
    terminal_before: DateTime<Utc>,
    cancel_grace_before: DateTime<Utc>,
    max_groups: i64,
    max_bytes: i64,
) -> fusillade_arsenal::error::Result<RetainedResponseArchiveOutcome> {
    let cutoffs = RetainedResponseArchiveCutoffs::new(
        terminal_before.max(cancel_grace_before),
        terminal_before,
        cancel_grace_before,
    )
    .expect("archive test cutoffs must be ordered");
    manager
        .archive_terminal_batchless_responses(policy, &cutoffs, max_groups, max_bytes)
        .await
}

async fn archive_clock(pool: &PgPool) -> (DateTime<Utc>, DateTime<Utc>) {
    sqlx::query_as(
        r#"
        SELECT statement_timestamp(),
               date_trunc('day', statement_timestamp() AT TIME ZONE 'UTC')
                   AT TIME ZONE 'UTC'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("archive clock must be readable")
}

#[sqlx::test]
async fn postgres_reports_exact_retained_response_index_readiness(pool: PgPool) {
    let manager = manager(&pool).await;
    assert!(manager.supports_retained_response_lifecycle());
    assert!(
        !manager
            .retained_response_archive_index_ready()
            .await
            .expect("missing candidate index must be reported")
    );

    sqlx::query("CREATE INDEX idx_requests_batchless_retention_due ON requests (id)")
        .execute(&pool)
        .await
        .expect("wrong-shape candidate index must install");
    assert!(
        !manager
            .retained_response_archive_index_ready()
            .await
            .expect("wrong-shape candidate index must be reported")
    );

    sqlx::query("DROP INDEX idx_requests_batchless_retention_due")
        .execute(&pool)
        .await
        .expect("wrong-shape candidate index must drop");

    sqlx::query(
        r#"
        CREATE INDEX idx_requests_batchless_retention_due
        ON requests (
          service_tier,
          (CASE state WHEN 'completed' THEN completed_at
                      WHEN 'failed' THEN failed_at
                      WHEN 'canceled' THEN canceled_at END),
          id
        )
        INCLUDE (response_body)
        WHERE batch_id IS NULL
          AND state IN ('completed', 'failed', 'canceled')
        "#,
    )
    .execute(&pool)
    .await
    .expect("candidate index with an included payload column must install");
    assert!(
        !manager
            .retained_response_archive_index_ready()
            .await
            .expect("included payload column must be reported"),
        "an index with an included payload column is not the exact operator prerequisite"
    );

    sqlx::query("DROP INDEX idx_requests_batchless_retention_due")
        .execute(&pool)
        .await
        .expect("included-column candidate index must drop");
    install_candidate_index(&pool).await;
    assert!(
        manager
            .retained_response_archive_index_ready()
            .await
            .expect("canonical candidate index must be reported")
    );
}

#[sqlx::test]
async fn continuous_ninety_day_runway_enables_new_graph_move(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (observed_at, _) = archive_clock(&pool).await;
    let retention_seconds = 90 * 86_400;
    let runway_days = 7;
    let policy = exact_policy(&[("flex", retention_seconds)]);
    let first_delete_on = observed_at
        .date_naive()
        .succ_opt()
        .expect("test clock must have a following day");
    let retention_horizon = RetentionPolicy::delete_on(observed_at, retention_seconds)
        .expect("retention horizon must be representable");
    let last_delete_on = retention_horizon
        .checked_add_signed(TimeDelta::days(i64::from(runway_days)))
        .expect("runway horizon must be representable");
    let expected_runway = (last_delete_on - first_delete_on).num_days() + 1;
    let manager = manager(&pool).await;

    let provisioned = manager
        .ensure_retained_response_partitions(&policy, runway_days)
        .await
        .expect("continuous runway must provision");
    assert_eq!(provisioned.created, expected_runway);
    assert_eq!(provisioned.contiguous_ahead, expected_runway);
    assert_eq!(provisioned.required, expected_runway);
    assert!(provisioned.is_complete());

    let idempotent = manager
        .ensure_retained_response_partitions(&policy, runway_days)
        .await
        .expect("continuous runway provisioning must be idempotent");
    assert_eq!(idempotent.created, 0);
    assert_eq!(idempotent.contiguous_ahead, expected_runway);
    assert_eq!(idempotent.required, expected_runway);
    assert!(idempotent.is_complete());

    let contiguous: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::BIGINT
        FROM generate_series($1::date, $2::date, INTERVAL '1 day') AS day(delete_on)
        WHERE EXISTS (
            SELECT 1
            FROM retained_response_buckets bucket
            JOIN pg_class child ON child.oid = bucket.partition_oid
            JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
            JOIN pg_inherits inheritance ON inheritance.inhrelid = child.oid
            WHERE bucket.delete_on = day.delete_on::date
              AND bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table = 'retained_response_objects_d'
                  || to_char(day.delete_on, 'YYYYMMDD')
              AND namespace.nspname = bucket.partition_schema
              AND child.relname = bucket.partition_table
              AND inheritance.inhparent = 'retained_response_objects'::regclass
              AND NOT inheritance.inhdetachpending
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)',
                  day.delete_on::date,
                  day.delete_on::date + 1
              )
        )
        "#,
    )
    .bind(first_delete_on)
    .bind(last_delete_on)
    .fetch_one(&pool)
    .await
    .expect("continuous runway must be inspectable");
    assert_eq!(contiguous, expected_runway);

    let terminal_at = observed_at - TimeDelta::hours(2);
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "continuous-runway",
    )
    .await;
    let outcome = archive_at(
        &manager,
        &policy,
        observed_at - TimeDelta::hours(1),
        observed_at,
        1,
        i64::MAX,
    )
    .await
    .expect("a newly eligible graph must move into the provisioned runway");
    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn newly_terminal_singleton_moves_before_expiry(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, _) = archive_clock(&pool).await;
    let terminal_at = archive_now - TimeDelta::hours(2);
    let terminal_before = archive_now - TimeDelta::hours(1);
    let retention_seconds = 30 * 86_400;
    let delete_on = RetentionPolicy::delete_on(terminal_at, retention_seconds).unwrap();
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "newly-terminal",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", retention_seconds)]),
        terminal_before,
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("a dwell-eligible nonexpired graph must move");

    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn graph_newer_than_terminal_cutoff_stays_wholly_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, _) = archive_clock(&pool).await;
    let terminal_at = archive_now - TimeDelta::minutes(30);
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "inside-dwell",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", 30 * 86_400)]),
        archive_now - TimeDelta::hours(1),
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("a graph inside the dwell period is not an error");

    assert_eq!(outcome, RetainedResponseArchiveOutcome::default());
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn mixed_timestamp_graph_waits_for_latest_member_then_moves(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, _) = archive_clock(&pool).await;
    let request_terminal = archive_now - TimeDelta::hours(3);
    let step_terminal = archive_now - TimeDelta::minutes(30);
    let retention_seconds = 30 * 86_400;
    let delete_on = RetentionPolicy::delete_on(step_terminal, retention_seconds).unwrap();
    ensure_partition(&pool, delete_on).await;
    let mut graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        request_terminal,
        "mixed-dwell",
    )
    .await;
    let head = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: head,
            request_id: Some(graph.request_ids[0]),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: step_terminal,
        },
    )
    .await;
    graph.group_id = head;
    graph.step_ids = vec![head];
    let manager = manager(&pool).await;
    let policy = exact_policy(&[("flex", retention_seconds)]);

    let waiting = archive_at(
        &manager,
        &policy,
        archive_now - TimeDelta::hours(1),
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("a graph with a member inside dwell is deferred");
    assert_eq!(waiting.groups_archived, 0);
    assert!(waiting.may_have_more);
    assert_wholly_live(&pool, &graph).await;

    let moved = archive_at(&manager, &policy, archive_now, archive_now, 1, i64::MAX)
        .await
        .expect("the complete graph moves once its latest member clears dwell");
    assert_eq!(moved.groups_archived, 1);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn due_legacy_graph_stays_live_and_does_not_report_discoverable_work(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, today_start) = archive_clock(&pool).await;
    let retention_seconds = 86_400;
    let terminal_at = today_start - TimeDelta::days(2);
    let due_delete_on = RetentionPolicy::delete_on(terminal_at, retention_seconds).unwrap();
    ensure_partition(&pool, due_delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "due-legacy",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", retention_seconds)]),
        archive_now,
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("ordinary movement ignores due legacy content");

    assert_eq!(outcome, RetainedResponseArchiveOutcome::default());
    assert_wholly_live(&pool, &graph).await;
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM retained_response_objects WHERE delete_on = $1")
            .bind(due_delete_on)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, 0);
}

#[sqlx::test]
async fn due_legacy_backlog_cannot_starve_an_older_nonexpired_tier(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, today_start) = archive_clock(&pool).await;
    let short_retention = 86_400;
    let long_retention = 30 * 86_400;
    let mut due_graphs = Vec::new();
    for offset in 0..12 {
        due_graphs.push(
            singleton(
                &pool,
                "flex",
                TerminalState::Completed,
                today_start - TimeDelta::days(2) + TimeDelta::minutes(offset),
                &format!("due-backlog-{offset}"),
            )
            .await,
        );
    }
    let eligible_terminal = today_start - TimeDelta::days(10);
    let eligible_delete_on = RetentionPolicy::delete_on(eligible_terminal, long_retention).unwrap();
    ensure_partition(&pool, eligible_delete_on).await;
    let eligible = singleton(
        &pool,
        "priority",
        TerminalState::Completed,
        eligible_terminal,
        "eligible-behind-due-backlog",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", short_retention), ("priority", long_retention)]),
        archive_now,
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("due roots cannot consume bounded discovery");

    assert_eq!(outcome.groups_archived, 1);
    assert!(!outcome.may_have_more);
    assert_wholly_retained(&pool, &eligible).await;
    for graph in &due_graphs {
        assert_wholly_live(&pool, graph).await;
    }
}

#[sqlx::test]
async fn seed_at_utc_window_lower_bound_is_discoverable(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, today_start) = archive_clock(&pool).await;
    let retention_seconds = 86_400;
    let terminal_at = today_start - TimeDelta::seconds(retention_seconds as i64);
    let delete_on = RetentionPolicy::delete_on(terminal_at, retention_seconds).unwrap();
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "utc-lower-bound",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", retention_seconds)]),
        archive_now,
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("the inclusive future-day boundary is movement eligible");

    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn old_seed_with_newer_step_is_conservatively_left_for_legacy_handling(pool: PgPool) {
    install_candidate_index(&pool).await;
    let (archive_now, today_start) = archive_clock(&pool).await;
    let retention_seconds = 86_400;
    let request_terminal = today_start - TimeDelta::days(2);
    let step_terminal = archive_now - TimeDelta::hours(1);
    let future_delete_on = RetentionPolicy::delete_on(step_terminal, retention_seconds).unwrap();
    ensure_partition(&pool, future_delete_on).await;
    let mut graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        request_terminal,
        "legacy-seed-new-step",
    )
    .await;
    let head = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: head,
            request_id: Some(graph.request_ids[0]),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: step_terminal,
        },
    )
    .await;
    graph.group_id = head;
    graph.step_ids = vec![head];
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &exact_policy(&[("flex", retention_seconds)]),
        archive_now,
        archive_now,
        1,
        i64::MAX,
    )
    .await
    .expect("a conservative discovery false negative is a safe no-op");

    assert_eq!(outcome, RetainedResponseArchiveOutcome::default());
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn stale_singleton_candidate_never_cascade_deletes_a_new_response_graph(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let mut graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "stale-singleton",
    )
    .await;
    let (gated_manager, held_connection, write_requested) = write_gated_manager(&pool).await;
    let retention_policy = policy(&[("flex", 86_400)]);
    let mut mover =
        tokio::spawn(async move { archive(&gated_manager, &retention_policy, 1, i64::MAX).await });

    tokio::select! {
        result = &mut mover => panic!("mover completed before the write gate: {result:?}"),
        () = wait_for_write_request(&write_requested) => {}
    }

    let head = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: head,
            request_id: Some(graph.request_ids[0]),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-01T10:00:00Z"),
        },
    )
    .await;
    insert_step(
        &pool,
        StepFixture {
            id: child,
            request_id: None,
            prev_step_id: Some(head),
            parent_step_id: Some(head),
            sequence: 2,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-01T11:00:00Z"),
        },
    )
    .await;
    graph.group_id = head;
    graph.step_ids = vec![head, child];
    graph.step_ids.sort_unstable();

    drop(held_connection);
    match mover.await.expect("mover task must finish") {
        Ok(outcome) if outcome.groups_archived == 1 => {
            assert_eq!(outcome.requests_archived, 1);
            assert_eq!(outcome.steps_archived, 2);
            assert_wholly_retained(&pool, &graph).await;
        }
        _ => assert_wholly_live(&pool, &graph).await,
    }
}

#[sqlx::test]
async fn nonhead_parent_cascade_edge_fails_closed_without_deleting_any_member(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let mut graph = branching_graph(&pool, TerminalState::Completed).await;
    let nonhead = *graph
        .step_ids
        .iter()
        .find(|step_id| **step_id != graph.group_id)
        .unwrap();
    let malformed = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: malformed,
            request_id: None,
            prev_step_id: Some(nonhead),
            parent_step_id: Some(nonhead),
            sequence: 99,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-03T15:00:00Z"),
        },
    )
    .await;
    graph.step_ids.push(malformed);
    graph.step_ids.sort_unstable();
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a transitive non-head parent edge must fail closed");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn cross_head_predecessor_cascade_edge_fails_closed_without_deleting_any_member(
    pool: PgPool,
) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let mut graph = branching_graph(&pool, TerminalState::Completed).await;
    let nonhead = *graph
        .step_ids
        .iter()
        .find(|step_id| **step_id != graph.group_id)
        .unwrap();
    let other = singleton(
        &pool,
        "background",
        TerminalState::Completed,
        timestamp("2026-08-04T10:00:00Z"),
        "cross-head-owner",
    )
    .await;
    let other_head = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: other_head,
            request_id: Some(other.request_ids[0]),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-04T10:00:00Z"),
        },
    )
    .await;
    let malformed = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: malformed,
            request_id: None,
            prev_step_id: Some(nonhead),
            parent_step_id: Some(other_head),
            sequence: 100,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-04T11:00:00Z"),
        },
    )
    .await;
    graph.request_ids.extend(other.request_ids.iter().copied());
    graph.request_ids.sort_unstable();
    graph
        .template_ids
        .extend(other.template_ids.iter().copied());
    graph.template_ids.sort_unstable();
    graph.step_ids.extend([other_head, malformed]);
    graph.step_ids.sort_unstable();
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a predecessor edge crossing response heads must fail closed");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn duplicate_loser_starting_after_winner_commit_returns_a_clean_noop(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "ordered-duplicate",
    )
    .await;
    let retention_policy = policy(&[("flex", 86_400)]);
    let (loser_manager, held_connection, write_requested) = write_gated_manager(&pool).await;
    let loser_policy = retention_policy.clone();
    let mut loser =
        tokio::spawn(async move { archive(&loser_manager, &loser_policy, 1, i64::MAX).await });

    tokio::select! {
        result = &mut loser => panic!("loser completed before the write gate: {result:?}"),
        () = wait_for_write_request(&write_requested) => {}
    }

    let winner_manager = manager(&pool).await;
    let winner = archive(&winner_manager, &retention_policy, 1, i64::MAX)
        .await
        .expect("winner must archive the live graph");
    assert_eq!(winner.groups_archived, 1);
    drop(held_connection);

    let loser = loser
        .await
        .expect("loser task must finish")
        .expect("a loser observing the committed winner must return a clean outcome");
    assert_eq!(loser.groups_archived, 0);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn partially_deleted_discovered_graph_never_returns_already_gone(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let head_request_id: Uuid = sqlx::query_scalar(
        "SELECT request_id FROM response_steps WHERE id = $1 AND request_id IS NOT NULL",
    )
    .bind(graph.group_id)
    .fetch_one(&pool)
    .await
    .expect("the response head must identify its request");
    let sibling_request_ids = graph
        .request_ids
        .iter()
        .copied()
        .filter(|request_id| *request_id != head_request_id)
        .collect::<Vec<_>>();
    assert_eq!(sibling_request_ids.len(), 1);

    let retention_policy = policy(&[("flex", 86_400), ("priority", 3 * 86_400)]);
    let (gated_manager, held_connection, write_requested) = write_gated_manager(&pool).await;
    let mover_policy = retention_policy.clone();
    let mut mover =
        tokio::spawn(async move { archive(&gated_manager, &mover_policy, 1, i64::MAX).await });

    tokio::select! {
        result = &mut mover => panic!("mover completed before the write gate: {result:?}"),
        () = wait_for_write_request(&write_requested) => {}
    }

    sqlx::query("DELETE FROM requests WHERE id = $1")
        .bind(head_request_id)
        .execute(&pool)
        .await
        .expect("the selected head request must delete after discovery");
    drop(held_connection);

    let error = mover
        .await
        .expect("mover task must finish")
        .expect_err("a partially remaining discovered graph must fail closed");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );

    assert_eq!(count_ids(&pool, "requests", &[head_request_id]).await, 0);
    assert_eq!(
        count_ids(&pool, "requests", &sibling_request_ids).await,
        sibling_request_ids.len() as i64
    );
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 0);
    assert_eq!(
        count_ids(&pool, "request_templates", &graph.template_ids).await,
        graph.template_ids.len() as i64
    );
    assert_eq!(retained_counts(&pool, graph.group_id).await, (0, 0, 0));
}

#[sqlx::test]
async fn deleting_head_between_seed_and_topology_discovery_never_returns_already_gone(
    pool: PgPool,
) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let head_request_id: Uuid = sqlx::query_scalar(
        "SELECT request_id FROM response_steps WHERE id = $1 AND request_id IS NOT NULL",
    )
    .bind(graph.group_id)
    .fetch_one(&pool)
    .await
    .expect("the response head must identify its request");
    let sibling_request_ids = graph
        .request_ids
        .iter()
        .copied()
        .filter(|request_id| *request_id != head_request_id)
        .collect::<Vec<_>>();
    assert_eq!(sibling_request_ids.len(), 1);

    let (gated_manager, held_connection, gate_requested) =
        discovery_read_gated_manager(&pool).await;
    let mut mover = tokio::spawn(async move {
        archive(&gated_manager, &policy(&[("flex", 86_400)]), 1, i64::MAX).await
    });

    tokio::select! {
        result = &mut mover => panic!("mover completed before the discovery gate: {result:?}"),
        () = wait_for_write_request(&gate_requested) => {}
    }

    sqlx::query("DELETE FROM requests WHERE id = $1")
        .bind(head_request_id)
        .execute(&pool)
        .await
        .expect("the selected head request must delete before topology expansion");
    drop(held_connection);

    let error = mover
        .await
        .expect("mover task must finish")
        .expect_err("a partially remaining discovery snapshot must fail closed");
    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );

    assert_eq!(count_ids(&pool, "requests", &[head_request_id]).await, 0);
    assert_eq!(
        count_ids(&pool, "requests", &sibling_request_ids).await,
        sibling_request_ids.len() as i64
    );
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 0);
    assert_eq!(
        count_ids(&pool, "request_templates", &graph.template_ids).await,
        graph.template_ids.len() as i64
    );
    assert_eq!(retained_counts(&pool, graph.group_id).await, (0, 0, 0));
}

#[sqlx::test]
async fn retirement_transition_fences_movement_until_retiring_is_visible(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "retirement-race",
    )
    .await;
    let mut retirement = pool.begin().await.unwrap();
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':'
                    || to_char($1::date, 'YYYYMMDD'),
                0
            )
        )
        "#,
    )
    .bind(delete_on)
    .execute(&mut *retirement)
    .await
    .unwrap();
    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(delete_on)
        .execute(&mut *retirement)
        .await
        .unwrap();

    let manager = manager(&pool).await;
    let retention_policy = policy(&[("flex", 86_400)]);
    let mut mover =
        tokio::spawn(async move { archive(&manager, &retention_policy, 1, i64::MAX).await });
    tokio::select! {
        result = &mut mover => panic!("mover crossed the uncommitted retirement fence: {result:?}"),
        () = wait_for_advisory_waiter(&pool) => {}
    }
    assert_wholly_live(&pool, &graph).await;

    retirement.commit().await.unwrap();
    let error = mover
        .await
        .expect("mover task must finish")
        .expect_err("the retiring state must be observed after the fence opens");
    assert_eq!(
        error.to_string(),
        "Retained response partition is unavailable"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn locked_oldest_group_does_not_consume_the_movement_budget(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let oldest = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "locked-lookahead",
    )
    .await;
    let next = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "movable-lookahead",
    )
    .await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM requests WHERE id = $1 FOR UPDATE")
        .bind(oldest.request_ids[0])
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("a locked oldest group must not hide the next group");

    assert_eq!(outcome.groups_archived, 1);
    assert!(outcome.skipped_locked);
    assert!(outcome.may_have_more);
    blocker.rollback().await.unwrap();
    assert_wholly_live(&pool, &oldest).await;
    assert_wholly_retained(&pool, &next).await;
}

#[sqlx::test]
async fn deferred_oldest_group_does_not_consume_the_movement_budget(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let mut deferred = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "deferred-lookahead",
    )
    .await;
    let future_terminal = timestamp("2026-09-01T00:00:00Z");
    let head = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: head,
            request_id: Some(deferred.request_ids[0]),
            prev_step_id: None,
            parent_step_id: None,
            sequence: 1,
            state: TerminalState::Completed,
            terminal_at: future_terminal,
        },
    )
    .await;
    deferred.group_id = head;
    deferred.step_ids = vec![head];
    let next = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "after-deferred-lookahead",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("a deferred oldest group must not hide the next group");

    assert_eq!(outcome.groups_archived, 1);
    assert!(outcome.may_have_more);
    assert_wholly_live(&pool, &deferred).await;
    assert_wholly_retained(&pool, &next).await;
}

#[sqlx::test]
async fn many_requests_in_one_graph_count_once_toward_the_group_budget(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-06")).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let mut first = branching_graph(&pool, TerminalState::Completed).await;
    let (third_request, third_template) = insert_request(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-02T12:00:00Z"),
        "third-request-in-graph",
    )
    .await;
    let third_step = Uuid::new_v4();
    insert_step(
        &pool,
        StepFixture {
            id: third_step,
            request_id: Some(third_request),
            prev_step_id: Some(first.group_id),
            parent_step_id: Some(first.group_id),
            sequence: 6,
            state: TerminalState::Completed,
            terminal_at: timestamp("2026-08-02T12:00:00Z"),
        },
    )
    .await;
    first.request_ids.push(third_request);
    first.template_ids.push(third_template);
    first.step_ids.push(third_step);
    for offset in 0_i64..6 {
        let terminal_at = timestamp("2026-08-02T12:10:00Z") + TimeDelta::minutes(offset);
        let suffix = format!("fanout-request-{offset}");
        let (request_id, template_id) = insert_request(
            &pool,
            "flex",
            TerminalState::Completed,
            terminal_at,
            &suffix,
        )
        .await;
        let step_id = Uuid::new_v4();
        insert_step(
            &pool,
            StepFixture {
                id: step_id,
                request_id: Some(request_id),
                prev_step_id: Some(first.group_id),
                parent_step_id: Some(first.group_id),
                sequence: 7 + offset,
                state: TerminalState::Completed,
                terminal_at,
            },
        )
        .await;
        first.request_ids.push(request_id);
        first.template_ids.push(template_id);
        first.step_ids.push(step_id);
    }
    first.request_ids.sort_unstable();
    first.template_ids.sort_unstable();
    first.step_ids.sort_unstable();
    let second = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-04T10:00:00Z"),
        "second-distinct-group",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        2,
        i64::MAX,
    )
    .await
    .expect("request fan-out must not consume distinct-group capacity");

    assert_eq!(outcome.groups_archived, 2);
    assert_wholly_retained(&pool, &first).await;
    assert_wholly_retained(&pool, &second).await;
}

#[sqlx::test]
async fn archives_singleton_request_template_as_one_group(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "singleton",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("singleton graph must archive");

    assert_eq!(outcome.groups_archived, 1);
    assert_eq!(outcome.requests_archived, 1);
    assert_eq!(outcome.steps_archived, 0);
    assert_eq!(outcome.templates_archived, 1);
    assert!(outcome.bytes_archived > 0);
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn archives_parallel_and_nested_tool_descendants_with_one_conservative_deadline(
    pool: PgPool,
) {
    install_candidate_index(&pool).await;
    // Latest member is 2026-08-03 14:00; the longest tier lasts three days,
    // so exact expiry is on Aug 6 and deletion starts strictly on Aug 7.
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let manager = manager(&pool).await;

    let outcome = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect("complete branching graph must archive");

    assert_eq!(outcome.groups_archived, 1);
    assert_eq!(outcome.requests_archived, 2);
    assert_eq!(outcome.steps_archived, 5);
    assert_eq!(outcome.templates_archived, 2);
    assert_wholly_retained(&pool, &graph).await;
    let delete_on: NaiveDate = sqlx::query_scalar(
        "SELECT MIN(delete_on) FROM retained_response_objects WHERE group_id = $1",
    )
    .bind(graph.group_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(delete_on, archive_date("2026-08-07"));
}

#[sqlx::test]
async fn locked_candidate_is_skipped_without_loading_or_moving_graph(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "locked",
    )
    .await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM requests WHERE id = $1 FOR UPDATE")
        .bind(graph.request_ids[0])
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("locked work is a normal skip");

    assert_eq!(outcome.groups_archived, 0);
    assert!(outcome.skipped_locked);
    blocker.rollback().await.unwrap();
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn nonterminal_member_fails_the_complete_graph_and_rolls_back(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Pending).await;
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a graph with one live member must fail closed");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn pending_request_with_terminal_step_fails_closed(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph =
        branching_graph_with_tail_states(&pool, TerminalState::Pending, TerminalState::Completed)
            .await;
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a pending request must fail even when its step is terminal");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn terminal_request_with_pending_step_fails_closed(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph =
        branching_graph_with_tail_states(&pool, TerminalState::Completed, TerminalState::Pending)
            .await;
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a pending step must fail even when its request is terminal");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn shared_template_fails_closed_with_every_reference_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let updated = sqlx::query(
        "UPDATE requests SET template_id = $1 WHERE id = ANY($2) AND template_id <> $1",
    )
    .bind(graph.template_ids[0])
    .bind(&graph.request_ids)
    .execute(&pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 1, "fixture must create one shared template");
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("a template shared inside the graph must not be moved or deleted");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn file_backed_template_fails_closed_with_the_file_reference_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "file-backed-template",
    )
    .await;
    let file_id = Uuid::new_v4();
    sqlx::query("INSERT INTO files (id, name) VALUES ($1, $2)")
        .bind(file_id)
        .bind("retention-file-backed-template.jsonl")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_templates SET file_id = $1 WHERE id = $2")
        .bind(file_id)
        .bind(graph.template_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("a file-backed template must not be moved or deleted");

    assert_eq!(
        RetainedResponseMaintenanceError::from_fusillade_error(&error),
        Some(RetainedResponseMaintenanceError::IncompleteGraph)
    );
    assert_wholly_live(&pool, &graph).await;
    assert_eq!(count_ids(&pool, "files", &[file_id]).await, 1);
}

#[sqlx::test]
async fn dispatched_cancellation_obeys_the_absolute_grace_cutoff(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-06")).await;
    ensure_partition(&pool, archive_date("2026-08-13")).await;
    let before_cutoff = singleton(
        &pool,
        "flex",
        TerminalState::Canceled { dispatched: true },
        timestamp("2026-08-05T00:00:00Z"),
        "canceled-before",
    )
    .await;
    let after_cutoff = singleton(
        &pool,
        "flex",
        TerminalState::Canceled { dispatched: true },
        timestamp("2026-08-12T00:00:00Z"),
        "canceled-after",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive_at(
        &manager,
        &policy(&[("flex", 1)]),
        timestamp("2026-08-13T00:00:00Z"),
        timestamp("2026-08-09T00:00:00Z"),
        2,
        i64::MAX,
    )
    .await
    .expect("grace-ineligible groups are deferred, not partially moved");

    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &before_cutoff).await;
    assert_wholly_live(&pool, &after_cutoff).await;
}

#[sqlx::test]
async fn missing_partition_fails_closed_with_the_whole_graph_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "missing-partition",
    )
    .await;
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("a missing daily partition must fail closed");

    assert_eq!(
        error.to_string(),
        "Retained response partition is unavailable"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn duplicate_movers_commit_exactly_one_copy(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "duplicate-mover",
    )
    .await;
    let first = manager(&pool).await;
    let second = manager(&pool).await;
    let policy = Arc::new(policy(&[("flex", 86_400)]));
    let barrier = Arc::new(Barrier::new(3));
    let first_task = {
        let barrier = Arc::clone(&barrier);
        let policy = Arc::clone(&policy);
        tokio::spawn(async move {
            barrier.wait().await;
            archive(&first, &policy, 1, i64::MAX).await
        })
    };
    let second_task = {
        let barrier = Arc::clone(&barrier);
        let policy = Arc::clone(&policy);
        tokio::spawn(async move {
            barrier.wait().await;
            archive(&second, &policy, 1, i64::MAX).await
        })
    };
    barrier.wait().await;
    let first_outcome = first_task.await.unwrap().unwrap();
    let second_outcome = second_task.await.unwrap().unwrap();

    assert_eq!(
        first_outcome.groups_archived + second_outcome.groups_archived,
        1
    );
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn exact_idempotent_replay_accepts_matching_objects_and_routes(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "exact-replay",
    )
    .await;
    sqlx::query(
        "CREATE TABLE retention_test_replay_templates AS SELECT * FROM request_templates WHERE id = ANY($1)",
    )
    .bind(&graph.template_ids)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE retention_test_replay_requests AS SELECT * FROM requests WHERE id = ANY($1)",
    )
    .bind(&graph.request_ids)
    .execute(&pool)
    .await
    .unwrap();
    let manager = manager(&pool).await;
    let policy = policy(&[("flex", 86_400)]);
    let first = archive(&manager, &policy, 1, i64::MAX).await.unwrap();
    assert_eq!(first.groups_archived, 1);

    sqlx::query("INSERT INTO request_templates SELECT * FROM retention_test_replay_templates")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO requests SELECT * FROM retention_test_replay_requests")
        .execute(&pool)
        .await
        .unwrap();
    let replay = archive(&manager, &policy, 1, i64::MAX)
        .await
        .expect("an exact object and route replay must be idempotent");

    assert_eq!(replay.groups_archived, 1);
    assert_eq!(replay.requests_archived, 1);
    assert_eq!(retained_counts(&pool, graph.group_id).await, (1, 1, 0));
    assert_wholly_retained(&pool, &graph).await;
}

#[sqlx::test]
async fn forced_insert_count_mismatch_rolls_back_every_object(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    sqlx::query(
        r#"
        CREATE FUNCTION retention_test_skip_step() RETURNS trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.object_kind = 'step' THEN
                RETURN NULL;
            END IF;
            RETURN NEW;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER retention_test_skip_step BEFORE INSERT ON retained_response_objects FOR EACH ROW EXECUTE FUNCTION retention_test_skip_step()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let manager = manager(&pool).await;

    let error = archive(
        &manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect_err("post-insert count mismatch must abort the graph");

    assert_eq!(
        error.to_string(),
        "Retained response integrity verification failed"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn forced_sha256_mismatch_rolls_back_every_object(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "digest-mismatch",
    )
    .await;
    sqlx::query(
        r#"
        CREATE FUNCTION retention_test_mutate_payload() RETURNS trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.object_kind = 'request' THEN
                -- Same byte length as "retention-test-model": byte-count
                -- verification alone cannot catch this mutation.
                NEW.payload = jsonb_set(
                    NEW.payload,
                    '{request,model}',
                    '"corrupted-test-model"'::jsonb
                );
            END IF;
            RETURN NEW;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER retention_test_mutate_payload BEFORE INSERT ON retained_response_objects FOR EACH ROW EXECUTE FUNCTION retention_test_mutate_payload()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("post-insert SHA-256 mismatch must abort the graph");

    assert_eq!(
        error.to_string(),
        "Retained response integrity verification failed"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn explicit_deletion_lock_racing_movement_leaves_no_partial_archive(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "delete-race",
    )
    .await;
    let mut deletion = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM requests WHERE id = $1")
        .bind(graph.request_ids[0])
        .execute(&mut *deletion)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("a deletion-held graph must be skipped");

    assert_eq!(outcome.groups_archived, 0);
    assert!(outcome.skipped_locked);
    deletion.rollback().await.unwrap();
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn late_completion_lock_racing_movement_keeps_the_graph_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Canceled { dispatched: true },
        timestamp("2026-08-01T10:00:00Z"),
        "late-completion",
    )
    .await;
    let mut completion = pool.begin().await.unwrap();
    sqlx::query(
        r#"
        UPDATE requests
        SET state = 'completed', response_status = 200,
            response_body = '{"late":true}', completed_at = $2,
            response_size = 13, routed_model = model
        WHERE id = $1
        "#,
    )
    .bind(graph.request_ids[0])
    .bind(timestamp("2026-08-14T00:00:00Z"))
    .execute(&mut *completion)
    .await
    .unwrap();
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("a late completion holding the request must win the lock race");

    assert_eq!(outcome.groups_archived, 0);
    assert!(outcome.skipped_locked);
    completion.commit().await.unwrap();
    assert_wholly_live(&pool, &graph).await;
    let state: String = sqlx::query_scalar("SELECT state FROM requests WHERE id = $1")
        .bind(graph.request_ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "completed");
}

#[sqlx::test]
async fn group_budget_stops_after_the_requested_number_of_graphs(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let first = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "budget-first",
    )
    .await;
    let second = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        "budget-second",
    )
    .await;
    let third = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "budget-third",
    )
    .await;
    let manager = manager(&pool).await;

    let outcome = archive(&manager, &policy(&[("flex", 86_400)]), 2, i64::MAX)
        .await
        .unwrap();

    assert_eq!(outcome.groups_archived, 2);
    assert!(outcome.may_have_more);
    assert_wholly_retained(&pool, &first).await;
    assert_wholly_retained(&pool, &second).await;
    assert_wholly_live(&pool, &third).await;
}

#[sqlx::test]
async fn byte_budget_defers_a_later_graph_but_allows_one_oversized_graph_alone(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let probe = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T07:00:00Z"),
        "small-payload",
    )
    .await;
    let manager = manager(&pool).await;
    let policy = policy(&[("flex", 86_400)]);
    let probe_outcome = archive(&manager, &policy, 1, i64::MAX).await.unwrap();
    assert_wholly_retained(&pool, &probe).await;

    let small = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T08:00:00Z"),
        "small-payload",
    )
    .await;
    let large_suffix = "x".repeat(8_192);
    let large = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T09:00:00Z"),
        &large_suffix,
    )
    .await;

    let bounded = archive(
        &manager,
        &policy,
        2,
        i64::try_from(probe_outcome.bytes_archived).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(bounded.groups_archived, 1);
    assert!(bounded.may_have_more);
    assert_wholly_retained(&pool, &small).await;
    assert_wholly_live(&pool, &large).await;

    let oversized = archive(&manager, &policy, 1, 1).await.unwrap();
    assert_eq!(oversized.groups_archived, 1);
    assert!(oversized.bytes_archived > 1);
    assert_wholly_retained(&pool, &large).await;
}

#[sqlx::test]
async fn nonpositive_limits_are_noops_even_without_candidate_index(pool: PgPool) {
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "nonpositive",
    )
    .await;
    let manager = manager(&pool).await;
    let policy = policy(&[("flex", 86_400)]);

    assert_eq!(
        archive(&manager, &policy, 0, 100).await.unwrap(),
        RetainedResponseArchiveOutcome::default()
    );
    assert_eq!(
        archive(&manager, &policy, 1, 0).await.unwrap(),
        RetainedResponseArchiveOutcome::default()
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn missing_validated_candidate_index_fails_closed(pool: PgPool) {
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "missing-index",
    )
    .await;
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("movement must be disabled without the exact candidate index");

    assert_eq!(
        error.to_string(),
        "Retained response candidate index is unavailable"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn retiring_partition_fails_closed_with_the_whole_graph_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(archive_date("2026-08-03"))
        .execute(&pool)
        .await
        .unwrap();
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "retiring-partition",
    )
    .await;
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("a retiring daily partition must fail closed");

    assert_eq!(
        error.to_string(),
        "Retained response partition is unavailable"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn conflicting_route_rolls_back_objects_and_keeps_the_graph_live(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    ensure_partition(&pool, archive_date("2026-08-04")).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "route-conflict",
    )
    .await;
    sqlx::query("INSERT INTO retained_response_group_routes (group_id, delete_on) VALUES ($1, $2)")
        .bind(graph.group_id)
        .bind(archive_date("2026-08-04"))
        .execute(&pool)
        .await
        .unwrap();
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("a differing retained route must fail closed");

    assert_eq!(
        error.to_string(),
        "Retained response integrity verification failed"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn extra_request_route_for_group_and_bucket_causes_integrity_rollback(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "extra-request-route",
    )
    .await;
    sqlx::query(
        "INSERT INTO retained_response_request_routes (request_id, group_id, delete_on) VALUES ($1, $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(graph.group_id)
    .bind(delete_on)
    .execute(&pool)
    .await
    .unwrap();
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("an extra request route must reject the retained graph");

    assert_eq!(
        error.to_string(),
        "Retained response integrity verification failed"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[sqlx::test]
async fn extra_step_route_for_group_and_bucket_causes_integrity_rollback(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-03");
    ensure_partition(&pool, delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "extra-step-route",
    )
    .await;
    sqlx::query(
        "INSERT INTO retained_response_step_routes (step_id, group_id, delete_on) VALUES ($1, $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(graph.group_id)
    .bind(delete_on)
    .execute(&pool)
    .await
    .unwrap();
    let manager = manager(&pool).await;

    let error = archive(&manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect_err("an extra step route must reject the retained graph");

    assert_eq!(
        error.to_string(),
        "Retained response integrity verification failed"
    );
    assert_wholly_live(&pool, &graph).await;
}

#[derive(Debug, PartialEq)]
struct PublicReadSnapshot {
    details: Vec<serde_json::Value>,
    step: serde_json::Value,
    step_by_request: serde_json::Value,
    chain: serde_json::Value,
    lists: Vec<(serde_json::Value, i64)>,
    flex_count: i64,
    trailing: Vec<fusillade_arsenal::manager::TrailingDemandCount>,
}

async fn capture_public_reads(
    request_manager: &PostgresRequestManager<TestDbPools>,
    step_manager: &PostgresResponseStepManager<TestDbPools>,
    graph: &LiveGraph,
) -> PublicReadSnapshot {
    let mut details = Vec::new();
    for request_id in &graph.request_ids {
        details.push(
            serde_json::to_value(
                request_manager
                    .get_request_detail(RequestId(*request_id))
                    .await
                    .expect("request detail must be readable"),
            )
            .unwrap(),
        );
    }

    let step = serde_json::to_value(
        step_manager
            .get_step(StepId(graph.group_id))
            .await
            .expect("step read must succeed")
            .expect("head step must exist"),
    )
    .unwrap();
    let step_by_request = serde_json::to_value(
        step_manager
            .get_step_by_request(RequestId(graph.request_ids[0]))
            .await
            .expect("request step read must succeed")
            .expect("model step must exist"),
    )
    .unwrap();
    let chain = serde_json::to_value(
        step_manager
            .list_chain(StepId(graph.group_id))
            .await
            .expect("chain read must succeed"),
    )
    .unwrap();

    let filters = vec![
        ListRequestsFilter::default(),
        ListRequestsFilter {
            active_first: true,
            ..Default::default()
        },
        ListRequestsFilter {
            created_by: Some(OWNER.to_owned()),
            status: Some("completed".to_owned()),
            models: Some(vec![MODEL.to_owned()]),
            created_after: Some(timestamp("2026-07-31T00:00:00Z")),
            created_before: Some(timestamp("2026-08-04T00:00:00Z")),
            service_tiers: Some(vec!["flex".to_owned()]),
            active_first: false,
            skip: 0,
            limit: 10,
        },
        ListRequestsFilter {
            skip: 0,
            limit: 1,
            ..Default::default()
        },
        ListRequestsFilter {
            skip: 1,
            limit: 1,
            ..Default::default()
        },
        ListRequestsFilter {
            skip: 2,
            limit: 1,
            ..Default::default()
        },
        ListRequestsFilter {
            models: Some(Vec::new()),
            ..Default::default()
        },
        ListRequestsFilter {
            service_tiers: Some(Vec::new()),
            ..Default::default()
        },
    ];
    let mut lists = Vec::new();
    for filter in filters {
        let result = request_manager
            .list_requests(filter)
            .await
            .expect("request list must succeed");
        lists.push((
            serde_json::to_value(result.data).unwrap(),
            result.total_count,
        ));
    }

    let flex_count = request_manager
        .count_owner_flex_requests_since(OWNER, timestamp("2026-07-01T00:00:00Z"), false)
        .await
        .expect("flex count must succeed");
    let mut trailing = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("retained-window".to_owned(), -31_536_000, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Any,
        )
        .await
        .expect("trailing demand must succeed");
    trailing.sort_by(|left, right| {
        (&left.model, &left.service_tier, &left.outcome).cmp(&(
            &right.model,
            &right.service_tier,
            &right.outcome,
        ))
    });

    PublicReadSnapshot {
        details,
        step,
        step_by_request,
        chain,
        lists,
        flex_count,
        trailing,
    }
}

#[sqlx::test]
async fn read_apis_preserve_exact_values_filters_pages_and_counts_after_move(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let _pending = singleton(
        &pool,
        "priority",
        TerminalState::Pending,
        timestamp("2026-08-04T10:00:00Z"),
        "still-live-pending",
    )
    .await;
    let (request_manager, step_manager) = managers(&pool).await;
    let before = capture_public_reads(&request_manager, &step_manager, &graph).await;

    assert_eq!(before.details.len(), 2);
    assert_eq!(before.chain.as_array().unwrap().len(), 5);
    assert_eq!(before.lists[0].1, 3);
    assert_eq!(before.lists[2].1, 1);
    assert_eq!(before.flex_count, 1);
    assert_eq!(before.trailing.iter().map(|row| row.count).sum::<i64>(), 2);

    let outcome = archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect("graph movement must succeed");
    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &graph).await;

    let after = capture_public_reads(&request_manager, &step_manager, &graph).await;
    assert_eq!(after, before);
}

#[sqlx::test]
async fn anomalous_request_chronology_uses_later_created_at_for_safe_deadline(pool: PgPool) {
    install_candidate_index(&pool).await;
    let expected_delete_on = archive_date("2026-08-07");
    ensure_partition(&pool, expected_delete_on).await;
    let graph = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "anomalous-created-after-terminal",
    )
    .await;
    let anomalous_created_at = timestamp("2026-08-05T10:00:00Z");
    sqlx::query("UPDATE requests SET created_at = $2 WHERE id = $1")
        .bind(graph.request_ids[0])
        .bind(anomalous_created_at)
        .execute(&pool)
        .await
        .unwrap();
    let request_manager = manager(&pool).await;

    let outcome = archive(&request_manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .expect("schema-valid anomalous chronology must archive conservatively");
    assert_eq!(outcome.groups_archived, 1);
    let routed_delete_on: NaiveDate = sqlx::query_scalar(
        "SELECT delete_on FROM retained_response_request_routes WHERE request_id = $1",
    )
    .bind(graph.request_ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(routed_delete_on, expected_delete_on);
    let detail = request_manager
        .get_request_detail(RequestId(graph.request_ids[0]))
        .await
        .expect("the safely retained anomalous row must decode");
    assert_eq!(detail.created_at, anomalous_created_at);
    assert_eq!(detail.completed_at, Some(timestamp("2026-08-01T10:00:00Z")));
}

async fn assert_list_ids(
    manager: &PostgresRequestManager<TestDbPools>,
    filter: ListRequestsFilter,
    expected_ids: &[Uuid],
) {
    let result = manager.list_requests(filter).await.unwrap();
    let actual_ids = result.data.iter().map(|row| row.id).collect::<Vec<_>>();
    assert_eq!(actual_ids, expected_ids);
    assert_eq!(result.total_count, expected_ids.len() as i64);
}

#[sqlx::test]
async fn retained_list_filters_each_discriminate_one_matching_request(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-12")).await;
    let terminal_at = timestamp("2026-08-10T10:00:00Z");
    let target = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "filter-target",
    )
    .await;
    let owner_distractor = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "filter-owner",
    )
    .await;
    sqlx::query("UPDATE requests SET created_by = 'other-owner' WHERE id = $1")
        .bind(owner_distractor.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let status_distractor = singleton(
        &pool,
        "flex",
        TerminalState::Failed,
        terminal_at,
        "filter-status",
    )
    .await;
    let model_distractor = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "filter-model",
    )
    .await;
    sqlx::query("UPDATE requests SET model = 'other-model' WHERE id = $1")
        .bind(model_distractor.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let tier_distractor = singleton(
        &pool,
        "priority",
        TerminalState::Completed,
        terminal_at,
        "filter-tier",
    )
    .await;
    let early_distractor = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "filter-created-after",
    )
    .await;
    sqlx::query("UPDATE requests SET created_at = '2026-08-10T06:00:00Z' WHERE id = $1")
        .bind(early_distractor.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let late_distractor = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        terminal_at,
        "filter-created-before",
    )
    .await;
    sqlx::query("UPDATE requests SET created_at = '2026-08-10T09:30:00Z' WHERE id = $1")
        .bind(late_distractor.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();

    let request_manager = manager(&pool).await;
    let outcome = archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 86_400)]),
        7,
        i64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(outcome.groups_archived, 7);

    let target_filter = ListRequestsFilter {
        created_by: Some(OWNER.to_owned()),
        status: Some("completed".to_owned()),
        models: Some(vec![MODEL.to_owned()]),
        created_after: Some(timestamp("2026-08-10T07:30:00Z")),
        created_before: Some(timestamp("2026-08-10T08:30:00Z")),
        service_tiers: Some(vec!["flex".to_owned()]),
        active_first: false,
        skip: 0,
        limit: 20,
    };
    assert_list_ids(
        &request_manager,
        target_filter.clone(),
        &[target.request_ids[0]],
    )
    .await;

    let mut owner_filter = target_filter.clone();
    owner_filter.created_by = Some("other-owner".to_owned());
    assert_list_ids(
        &request_manager,
        owner_filter,
        &[owner_distractor.request_ids[0]],
    )
    .await;
    let mut status_filter = target_filter.clone();
    status_filter.status = Some("failed".to_owned());
    assert_list_ids(
        &request_manager,
        status_filter,
        &[status_distractor.request_ids[0]],
    )
    .await;
    let mut model_filter = target_filter.clone();
    model_filter.models = Some(vec!["other-model".to_owned()]);
    assert_list_ids(
        &request_manager,
        model_filter,
        &[model_distractor.request_ids[0]],
    )
    .await;
    let mut tier_filter = target_filter.clone();
    tier_filter.service_tiers = Some(vec!["priority".to_owned()]);
    assert_list_ids(
        &request_manager,
        tier_filter,
        &[tier_distractor.request_ids[0]],
    )
    .await;
    let mut after_filter = target_filter.clone();
    after_filter.created_after = Some(timestamp("2026-08-10T09:00:00Z"));
    after_filter.created_before = Some(timestamp("2026-08-10T10:00:00Z"));
    assert_list_ids(
        &request_manager,
        after_filter,
        &[late_distractor.request_ids[0]],
    )
    .await;
    let mut before_filter = target_filter;
    before_filter.created_after = Some(timestamp("2026-08-10T05:00:00Z"));
    before_filter.created_before = Some(timestamp("2026-08-10T07:00:00Z"));
    assert_list_ids(
        &request_manager,
        before_filter,
        &[early_distractor.request_ids[0]],
    )
    .await;
}

#[sqlx::test]
async fn retained_trailing_filters_discriminate_windows_models_and_tiers(pool: PgPool) {
    install_candidate_index(&pool).await;
    let fixture_now = Utc::now();
    let target_terminal = fixture_now - TimeDelta::hours(2);
    let older_terminal = fixture_now - TimeDelta::hours(8);
    let target = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        target_terminal,
        "trailing-target",
    )
    .await;
    let older = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        older_terminal,
        "trailing-older",
    )
    .await;
    let priority = singleton(
        &pool,
        "priority",
        TerminalState::Completed,
        target_terminal,
        "trailing-priority",
    )
    .await;
    let other_model = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        target_terminal,
        "trailing-model",
    )
    .await;
    sqlx::query("UPDATE requests SET model = 'other-model' WHERE id = $1")
        .bind(other_model.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let retention_policy = policy(&[("flex", 1), ("priority", 1)]);
    let retention_seconds = retention_policy.batchless_seconds_by_service_tier["flex"];
    for terminal_at in [target_terminal, older_terminal] {
        let delete_on = RetentionPolicy::delete_on(terminal_at, retention_seconds).unwrap();
        ensure_partition(&pool, delete_on).await;
    }
    let request_manager = manager(&pool).await;
    let archive_now = Utc::now();
    let outcome = archive_at(
        &request_manager,
        &retention_policy,
        archive_now,
        archive_now,
        4,
        i64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(outcome.groups_archived, 4);

    let mut flex_rows = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[
                ("recent".to_owned(), -4 * 3_600, 0),
                ("older".to_owned(), -10 * 3_600, -6 * 3_600),
            ],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Include(vec![Some("flex".to_owned())]),
        )
        .await
        .unwrap();
    flex_rows.sort_by(|left, right| left.window_label.cmp(&right.window_label));
    assert_eq!(flex_rows.len(), 2);
    assert_eq!(flex_rows[0].window_label, "older");
    assert_eq!(flex_rows[0].service_tier.as_deref(), Some("flex"));
    assert_eq!(flex_rows[0].count, 1);
    assert_eq!(flex_rows[1].window_label, "recent");
    assert_eq!(flex_rows[1].service_tier.as_deref(), Some("flex"));
    assert_eq!(flex_rows[1].count, 1);

    let priority_rows = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("recent".to_owned(), -4 * 3_600, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Include(vec![Some("priority".to_owned())]),
        )
        .await
        .unwrap();
    assert_eq!(priority_rows.len(), 1);
    assert_eq!(priority_rows[0].service_tier.as_deref(), Some("priority"));
    assert_eq!(priority_rows[0].count, 1);

    let excluded_flex = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("recent".to_owned(), -4 * 3_600, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Exclude(vec![Some("flex".to_owned())]),
        )
        .await
        .unwrap();
    assert_eq!(excluded_flex, priority_rows);
    assert_wholly_retained(&pool, &target).await;
    assert_wholly_retained(&pool, &older).await;
    assert_wholly_retained(&pool, &priority).await;
    assert_wholly_retained(&pool, &other_model).await;
}

#[sqlx::test]
async fn retained_failed_trailing_filters_discriminate_windows_models_and_tiers(pool: PgPool) {
    install_candidate_index(&pool).await;
    let fixture_now = Utc::now();
    let target_terminal = fixture_now - TimeDelta::hours(2);
    let older_terminal = fixture_now - TimeDelta::hours(8);
    let target = singleton(
        &pool,
        "flex",
        TerminalState::Failed,
        target_terminal,
        "failed-trailing-target",
    )
    .await;
    let older = singleton(
        &pool,
        "flex",
        TerminalState::Failed,
        older_terminal,
        "failed-trailing-older",
    )
    .await;
    let priority = singleton(
        &pool,
        "priority",
        TerminalState::Failed,
        target_terminal,
        "failed-trailing-priority",
    )
    .await;
    let other_model = singleton(
        &pool,
        "flex",
        TerminalState::Failed,
        target_terminal,
        "failed-trailing-model",
    )
    .await;
    sqlx::query("UPDATE requests SET model = 'other-model' WHERE id = $1")
        .bind(other_model.request_ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let retention_policy = policy(&[("flex", 1), ("priority", 1)]);
    let retention_seconds = retention_policy.batchless_seconds_by_service_tier["flex"];
    for terminal_at in [target_terminal, older_terminal] {
        let delete_on = RetentionPolicy::delete_on(terminal_at, retention_seconds).unwrap();
        ensure_partition(&pool, delete_on).await;
    }
    let request_manager = manager(&pool).await;
    let archive_now = Utc::now();
    let outcome = archive_at(
        &request_manager,
        &retention_policy,
        archive_now,
        archive_now,
        4,
        i64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(outcome.groups_archived, 4);

    let mut flex_rows = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[
                ("recent".to_owned(), -4 * 3_600, 0),
                ("older".to_owned(), -10 * 3_600, -6 * 3_600),
            ],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Include(vec![Some("flex".to_owned())]),
        )
        .await
        .unwrap();
    flex_rows.sort_by(|left, right| left.window_label.cmp(&right.window_label));
    assert_eq!(flex_rows.len(), 2);
    assert_eq!(flex_rows[0].window_label, "older");
    assert_eq!(flex_rows[0].model, MODEL);
    assert_eq!(flex_rows[0].service_tier.as_deref(), Some("flex"));
    assert_eq!(flex_rows[0].outcome, "failed");
    assert_eq!(flex_rows[0].count, 1);
    assert_eq!(flex_rows[1].window_label, "recent");
    assert_eq!(flex_rows[1].model, MODEL);
    assert_eq!(flex_rows[1].service_tier.as_deref(), Some("flex"));
    assert_eq!(flex_rows[1].outcome, "failed");
    assert_eq!(flex_rows[1].count, 1);

    let priority_rows = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("recent".to_owned(), -4 * 3_600, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Include(vec![Some("priority".to_owned())]),
        )
        .await
        .unwrap();
    assert_eq!(priority_rows.len(), 1);
    assert_eq!(priority_rows[0].model, MODEL);
    assert_eq!(priority_rows[0].service_tier.as_deref(), Some("priority"));
    assert_eq!(priority_rows[0].outcome, "failed");
    assert_eq!(priority_rows[0].count, 1);

    let excluded_flex = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("recent".to_owned(), -4 * 3_600, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Exclude(vec![Some("flex".to_owned())]),
        )
        .await
        .unwrap();
    assert_eq!(excluded_flex, priority_rows);

    let other_model_rows = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("recent".to_owned(), -4 * 3_600, 0)],
            &["other-model".to_owned()],
            &ServiceTierFilter::Include(vec![Some("flex".to_owned())]),
        )
        .await
        .unwrap();
    assert_eq!(other_model_rows.len(), 1);
    assert_eq!(other_model_rows[0].model, "other-model");
    assert_eq!(other_model_rows[0].service_tier.as_deref(), Some("flex"));
    assert_eq!(other_model_rows[0].outcome, "failed");
    assert_eq!(other_model_rows[0].count, 1);

    assert_wholly_retained(&pool, &target).await;
    assert_wholly_retained(&pool, &older).await;
    assert_wholly_retained(&pool, &priority).await;
    assert_wholly_retained(&pool, &other_model).await;
}

#[sqlx::test]
async fn read_point_apis_never_observe_partial_data_during_atomic_movement(pool: PgPool) {
    const MOVEMENT_GATE_KEY: i64 = 730_006;

    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let (request_manager, step_manager) = managers(&pool).await;
    let before = capture_public_reads(&request_manager, &step_manager, &graph).await;

    sqlx::query(
        r#"
        CREATE FUNCTION test_gate_retained_response_movement()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            PERFORM pg_advisory_xact_lock(730006);
            RETURN NULL;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .expect("movement gate function must install");
    sqlx::query(
        r#"
        CREATE TRIGGER test_gate_retained_response_movement
        BEFORE DELETE ON response_steps
        FOR EACH STATEMENT
        EXECUTE FUNCTION test_gate_retained_response_movement()
        "#,
    )
    .execute(&pool)
    .await
    .expect("movement gate trigger must install");

    let mut gate = pool
        .acquire()
        .await
        .expect("movement gate connection must be available");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MOVEMENT_GATE_KEY)
        .execute(&mut *gate)
        .await
        .expect("movement gate must lock");
    let mover_manager = manager(&pool).await;
    let mut mover = tokio::spawn(async move {
        archive(
            &mover_manager,
            &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
            1,
            i64::MAX,
        )
        .await
    });
    tokio::select! {
        result = &mut mover => panic!("movement completed before its deterministic gate: {result:?}"),
        () = wait_for_advisory_waiter_key(&pool, MOVEMENT_GATE_KEY) => {}
    }

    // The mover has copied every retained object and route in its transaction
    // and is paused immediately before deleting live steps. Other snapshots
    // must still see the complete live graph, never the uncommitted archive.
    let during = capture_public_reads(&request_manager, &step_manager, &graph).await;
    assert_eq!(during, before);

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(MOVEMENT_GATE_KEY)
        .fetch_one(&mut *gate)
        .await
        .expect("movement gate must unlock");
    assert!(unlocked);
    let outcome = mover
        .await
        .expect("movement task must finish")
        .expect("movement must commit after the gate opens");
    assert_eq!(outcome.groups_archived, 1);

    let after = capture_public_reads(&request_manager, &step_manager, &graph).await;
    assert_eq!(after, before);
}

async fn set_bucket_state(pool: &PgPool, delete_on: NaiveDate, state: &str) {
    sqlx::query("UPDATE retained_response_buckets SET state = $2 WHERE delete_on = $1")
        .bind(delete_on)
        .bind(state)
        .execute(pool)
        .await
        .expect("bucket state transition must succeed");
}

#[sqlx::test]
async fn read_point_helpers_keep_one_primary_snapshot_across_route_lookup(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-07");
    ensure_partition(&pool, delete_on).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let (request_manager, step_manager) = managers(&pool).await;
    archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();

    let expected_detail = serde_json::to_value(
        request_manager
            .get_request_detail(RequestId(graph.request_ids[0]))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut route_lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE retained_response_request_routes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *route_lock)
        .await
        .unwrap();
    let detail_manager = manager(&pool).await;
    let request_id = RequestId(graph.request_ids[0]);
    let mut detail_read =
        tokio::spawn(async move { detail_manager.get_request_detail(request_id).await });
    tokio::select! {
        result = &mut detail_read => panic!("detail read crossed its route gate: {result:?}"),
        () = wait_for_relation_waiter(&pool, "retained_response_request_routes") => {}
    }
    set_bucket_state(&pool, delete_on, "retiring").await;
    route_lock.commit().await.unwrap();
    assert_eq!(
        serde_json::to_value(detail_read.await.unwrap().unwrap()).unwrap(),
        expected_detail,
        "the retained detail must come from the snapshot established by the live lookup"
    );
    set_bucket_state(&pool, delete_on, "active").await;

    let expected_step =
        serde_json::to_value(step_manager.get_step(StepId(graph.group_id)).await.unwrap()).unwrap();
    let mut route_lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE retained_response_step_routes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *route_lock)
        .await
        .unwrap();
    let (_, gated_step_manager) = managers(&pool).await;
    let step_id = StepId(graph.group_id);
    let mut step_read = tokio::spawn(async move { gated_step_manager.get_step(step_id).await });
    tokio::select! {
        result = &mut step_read => panic!("step read crossed its route gate: {result:?}"),
        () = wait_for_relation_waiter(&pool, "retained_response_step_routes") => {}
    }
    set_bucket_state(&pool, delete_on, "retiring").await;
    route_lock.commit().await.unwrap();
    assert_eq!(
        serde_json::to_value(step_read.await.unwrap().unwrap()).unwrap(),
        expected_step
    );
    set_bucket_state(&pool, delete_on, "active").await;

    let expected_request_step = serde_json::to_value(
        step_manager
            .get_step_by_request(RequestId(graph.request_ids[0]))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut route_lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE retained_response_request_routes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *route_lock)
        .await
        .unwrap();
    let (_, gated_step_manager) = managers(&pool).await;
    let request_id = RequestId(graph.request_ids[0]);
    let mut request_step_read =
        tokio::spawn(async move { gated_step_manager.get_step_by_request(request_id).await });
    tokio::select! {
        result = &mut request_step_read => panic!("step-by-request read crossed its route gate: {result:?}"),
        () = wait_for_relation_waiter(&pool, "retained_response_request_routes") => {}
    }
    set_bucket_state(&pool, delete_on, "retiring").await;
    route_lock.commit().await.unwrap();
    assert_eq!(
        serde_json::to_value(request_step_read.await.unwrap().unwrap()).unwrap(),
        expected_request_step
    );
    set_bucket_state(&pool, delete_on, "active").await;

    let expected_chain = serde_json::to_value(
        step_manager
            .list_chain(StepId(graph.group_id))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut route_lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE retained_response_group_routes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *route_lock)
        .await
        .unwrap();
    let (_, gated_step_manager) = managers(&pool).await;
    let head_step_id = StepId(graph.group_id);
    let mut chain_read =
        tokio::spawn(async move { gated_step_manager.list_chain(head_step_id).await });
    tokio::select! {
        result = &mut chain_read => panic!("chain read crossed its route gate: {result:?}"),
        () = wait_for_relation_waiter(&pool, "retained_response_group_routes") => {}
    }
    set_bucket_state(&pool, delete_on, "retiring").await;
    route_lock.commit().await.unwrap();
    assert_eq!(
        serde_json::to_value(chain_read.await.unwrap().unwrap()).unwrap(),
        expected_chain
    );
}

#[sqlx::test]
async fn read_mixed_live_and_retained_rows_do_not_double_count_or_split_demand(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-03")).await;
    let first = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-01T10:00:00Z"),
        "mixed-first",
    )
    .await;
    let second = singleton(
        &pool,
        "flex",
        TerminalState::Completed,
        timestamp("2026-08-02T10:00:00Z"),
        "mixed-second",
    )
    .await;
    let request_manager = manager(&pool).await;

    let before_list = request_manager
        .list_requests(ListRequestsFilter::default())
        .await
        .unwrap();
    let before_flex = request_manager
        .count_owner_flex_requests_since(OWNER, timestamp("2026-07-01T00:00:00Z"), false)
        .await
        .unwrap();
    let before_trailing = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("retained-window".to_owned(), -31_536_000, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Any,
        )
        .await
        .unwrap();
    assert_eq!(before_list.total_count, 2);
    assert_eq!(before_flex, 2);
    assert_eq!(before_trailing.len(), 1);
    assert_eq!(before_trailing[0].count, 2);

    let outcome = archive(&request_manager, &policy(&[("flex", 86_400)]), 1, i64::MAX)
        .await
        .unwrap();
    assert_eq!(outcome.groups_archived, 1);
    assert_wholly_retained(&pool, &first).await;
    assert_wholly_live(&pool, &second).await;

    // Recreate the retained request's live identity to model a stale route or
    // repair overlap. Public unions must prefer this live row and suppress the
    // retained copy rather than double-counting one logical request.
    sqlx::query(
        r#"
        INSERT INTO request_templates (
            id, file_id, endpoint, method, path, body, model, api_key,
            line_number, body_byte_size, created_at, updated_at
        ) VALUES (
            $1, NULL, 'http://retention.invalid', 'POST', '/v1/responses',
            '{"prompt":"duplicate"}', $2, 'secret-test-key', 0, 22,
            '2026-08-01 08:00:00Z', '2026-08-01 10:00:00Z'
        )
        "#,
    )
    .bind(first.template_ids[0])
    .bind(MODEL)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO requests (
            id, batch_id, template_id, model, state, retry_attempt,
            claimed_at, started_at, response_status, response_body,
            completed_at, response_size, routed_model, service_tier,
            created_by, created_at, updated_at
        ) VALUES (
            $1, NULL, $2, $3, 'completed', 2,
            '2026-08-01 09:58:00Z', '2026-08-01 09:59:00Z', 200,
            '{"answer":"duplicate"}', '2026-08-01 10:00:00Z', 22,
            $3, 'flex', $4, '2026-08-01 08:00:00Z', '2026-08-01 10:00:00Z'
        )
        "#,
    )
    .bind(first.request_ids[0])
    .bind(first.template_ids[0])
    .bind(MODEL)
    .bind(OWNER)
    .execute(&pool)
    .await
    .unwrap();

    let after_list = request_manager
        .list_requests(ListRequestsFilter::default())
        .await
        .unwrap();
    let after_flex = request_manager
        .count_owner_flex_requests_since(OWNER, timestamp("2026-07-01T00:00:00Z"), false)
        .await
        .unwrap();
    let after_trailing = request_manager
        .get_completed_request_counts_by_model_and_window(
            &[("retained-window".to_owned(), -31_536_000, 0)],
            &[MODEL.to_owned()],
            &ServiceTierFilter::Any,
        )
        .await
        .unwrap();
    assert_eq!(after_list.total_count, 2);
    assert_eq!(after_list.data.len(), 2);
    assert_eq!(after_flex, 2);
    assert_eq!(after_trailing, before_trailing);
}

async fn assert_graph_reads_not_found(
    request_manager: &PostgresRequestManager<TestDbPools>,
    step_manager: &PostgresResponseStepManager<TestDbPools>,
    graph: &LiveGraph,
) {
    for request_id in &graph.request_ids {
        let error = request_manager
            .get_request_detail(RequestId(*request_id))
            .await
            .expect_err("request content must be fenced");
        assert!(
            matches!(error, fusillade_arsenal::error::FusilladeError::RequestNotFound(id) if id == RequestId(*request_id))
        );
        assert!(
            step_manager
                .get_step_by_request(RequestId(*request_id))
                .await
                .expect("step-by-request fence must not error")
                .is_none()
        );
    }
    for step_id in &graph.step_ids {
        assert!(
            step_manager
                .get_step(StepId(*step_id))
                .await
                .expect("step fence must not error")
                .is_none()
        );
    }
    assert!(
        step_manager
            .list_chain(StepId(graph.group_id))
            .await
            .expect("chain fence must not error")
            .is_empty()
    );
}

#[sqlx::test]
async fn read_apis_fail_closed_for_retiring_and_dropped_buckets(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-07");
    ensure_partition(&pool, delete_on).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let (request_manager, step_manager) = managers(&pool).await;
    archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .expect("graph movement must succeed");

    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(delete_on)
        .execute(&pool)
        .await
        .unwrap();
    assert_graph_reads_not_found(&request_manager, &step_manager, &graph).await;
    assert_eq!(
        request_manager
            .list_requests(ListRequestsFilter::default())
            .await
            .unwrap()
            .total_count,
        0
    );
    assert_eq!(
        request_manager
            .count_owner_flex_requests_since(OWNER, timestamp("2026-07-01T00:00:00Z"), false,)
            .await
            .unwrap(),
        0
    );
    assert!(
        request_manager
            .get_completed_request_counts_by_model_and_window(
                &[("retained-window".to_owned(), -31_536_000, 0)],
                &[MODEL.to_owned()],
                &ServiceTierFilter::Any,
            )
            .await
            .unwrap()
            .is_empty()
    );

    sqlx::query(
        "ALTER TABLE retained_response_objects DETACH PARTITION retained_response_objects_d20260906",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DROP TABLE retained_response_objects_d20260906")
        .execute(&pool)
        .await
        .unwrap();
    assert_graph_reads_not_found(&request_manager, &step_manager, &graph).await;
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 0);
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 0);
    assert_eq!(
        count_ids(&pool, "request_templates", &graph.template_ids).await,
        0
    );
    assert_eq!(retained_counts(&pool, graph.group_id).await, (0, 0, 0));
}

#[sqlx::test]
async fn read_point_routes_fail_closed_for_wrong_bucket_group_and_partition_oid(pool: PgPool) {
    install_candidate_index(&pool).await;
    let delete_on = archive_date("2026-08-07");
    let wrong_delete_on = archive_date("2026-08-08");
    ensure_partition(&pool, delete_on).await;
    ensure_partition(&pool, wrong_delete_on).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let (request_manager, step_manager) = managers(&pool).await;
    archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();
    let request_id = RequestId(graph.request_ids[0]);
    request_manager
        .get_request_detail(request_id)
        .await
        .expect("valid retained route must resolve");

    sqlx::query("UPDATE retained_response_request_routes SET delete_on = $2 WHERE request_id = $1")
        .bind(request_id.0)
        .bind(wrong_delete_on)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        request_manager.get_request_detail(request_id).await,
        Err(fusillade_arsenal::error::FusilladeError::RequestNotFound(id)) if id == request_id
    ));

    sqlx::query(
        "UPDATE retained_response_request_routes SET delete_on = $2, group_id = $3 WHERE request_id = $1",
    )
    .bind(request_id.0)
    .bind(delete_on)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        request_manager.get_request_detail(request_id).await,
        Err(fusillade_arsenal::error::FusilladeError::RequestNotFound(id)) if id == request_id
    ));

    sqlx::query("UPDATE retained_response_request_routes SET group_id = $2 WHERE request_id = $1")
        .bind(request_id.0)
        .bind(graph.group_id)
        .execute(&pool)
        .await
        .unwrap();

    let step_id = StepId(graph.group_id);
    step_manager
        .get_step(step_id)
        .await
        .unwrap()
        .expect("valid retained step route must resolve");
    sqlx::query("UPDATE retained_response_step_routes SET delete_on = $2 WHERE step_id = $1")
        .bind(step_id.0)
        .bind(wrong_delete_on)
        .execute(&pool)
        .await
        .unwrap();
    assert!(step_manager.get_step(step_id).await.unwrap().is_none());
    sqlx::query(
        "UPDATE retained_response_step_routes SET delete_on = $2, group_id = $3 WHERE step_id = $1",
    )
    .bind(step_id.0)
    .bind(delete_on)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    assert!(step_manager.get_step(step_id).await.unwrap().is_none());
    sqlx::query("UPDATE retained_response_step_routes SET group_id = $2 WHERE step_id = $1")
        .bind(step_id.0)
        .bind(graph.group_id)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("UPDATE retained_response_group_routes SET delete_on = $2 WHERE group_id = $1")
        .bind(graph.group_id)
        .bind(wrong_delete_on)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        request_manager.get_request_detail(request_id).await,
        Err(fusillade_arsenal::error::FusilladeError::RequestNotFound(id)) if id == request_id
    ));
    assert!(step_manager.get_step(step_id).await.unwrap().is_none());
    assert!(step_manager.list_chain(step_id).await.unwrap().is_empty());
    sqlx::query("UPDATE retained_response_group_routes SET delete_on = $2 WHERE group_id = $1")
        .bind(graph.group_id)
        .bind(delete_on)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "UPDATE retained_response_buckets SET partition_oid = 'retained_response_objects'::regclass::oid WHERE delete_on = $1",
    )
    .bind(delete_on)
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        request_manager.get_request_detail(request_id).await,
        Err(fusillade_arsenal::error::FusilladeError::RequestNotFound(id)) if id == request_id
    ));
    assert!(step_manager.get_step(step_id).await.unwrap().is_none());
}

#[sqlx::test]
async fn read_response_step_mutations_reject_retained_routes_without_content(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let (request_manager, step_manager) = managers(&pool).await;
    archive(
        &request_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();
    let step_id = StepId(graph.group_id);

    let create_error = step_manager
        .create_step(CreateStepInput {
            id: Some(step_id.0),
            request_id: None,
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ToolCall,
            step_sequence: 99,
            request_payload: json!({"must_not": "leak-or-insert"}),
        })
        .await
        .expect_err("retained step id must not be recreated");
    let mut errors = vec![create_error];
    errors.push(
        step_manager
            .create_step(CreateStepInput {
                id: Some(Uuid::new_v4()),
                request_id: None,
                prev_step_id: Some(step_id),
                parent_step_id: Some(step_id),
                step_kind: StepKind::ToolCall,
                step_sequence: 100,
                request_payload: json!({"must_not": "leak-or-link"}),
            })
            .await
            .expect_err("a new child must not attach to a retained graph"),
    );
    errors.push(
        step_manager
            .mark_step_processing(step_id)
            .await
            .expect_err("retained step must not be processed"),
    );
    errors.push(
        step_manager
            .complete_step(step_id, json!({"must_not": "leak"}))
            .await
            .expect_err("retained step must not be completed"),
    );
    errors.push(
        step_manager
            .fail_step(step_id, json!({"must_not": "leak"}))
            .await
            .expect_err("retained step must not be failed"),
    );
    errors.push(
        step_manager
            .cancel_step(step_id)
            .await
            .expect_err("retained step must not be canceled"),
    );
    errors.push(
        step_manager
            .requeue_step_for_retry(step_id)
            .await
            .expect_err("retained step must not be requeued"),
    );
    for error in errors {
        assert_eq!(error.to_string(), "Response step is already retained");
        match &error {
            fusillade_arsenal::error::FusilladeError::Other(source) => assert!(
                source
                    .downcast_ref::<RetainedResponseStepConflict>()
                    .is_some(),
                "retained mutations must return the typed conflict"
            ),
            other => panic!("unexpected retained mutation error: {other:?}"),
        }
        let debug = format!("{error:?}");
        assert!(!debug.contains(&step_id.0.to_string()));
        assert!(!debug.contains("must_not"));
    }
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 0);
}

#[sqlx::test]
async fn create_step_rolls_back_when_movement_commits_route_during_blocked_insert(pool: PgPool) {
    const MOVEMENT_AFTER_DELETE_GATE: i64 = 730_016;

    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let retained_step_id = StepId(graph.group_id);

    sqlx::query(
        r#"
        CREATE FUNCTION test_gate_after_retained_step_delete()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            PERFORM pg_advisory_xact_lock(730016);
            RETURN NULL;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TRIGGER test_gate_after_retained_step_delete
        AFTER DELETE ON response_steps
        FOR EACH STATEMENT
        EXECUTE FUNCTION test_gate_after_retained_step_delete()
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut movement_gate = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MOVEMENT_AFTER_DELETE_GATE)
        .execute(&mut *movement_gate)
        .await
        .unwrap();
    let mover_manager = manager(&pool).await;
    let mut mover = tokio::spawn(async move {
        archive(
            &mover_manager,
            &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
            1,
            i64::MAX,
        )
        .await
    });
    tokio::select! {
        result = &mut mover => panic!("movement completed before the after-delete gate: {result:?}"),
        () = wait_for_advisory_waiter_key(&pool, MOVEMENT_AFTER_DELETE_GATE) => {}
    }

    // The mover has inserted the active routes and deleted the old step in its
    // uncommitted transaction. The creator cannot see that route in its first
    // statement and then waits on the deleted primary-key tuple.
    let creator_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let creator_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&creator_pool)
        .await
        .unwrap();
    let creator_manager = PostgresResponseStepManager::new(WriteSignalingPools {
        read: creator_pool.clone(),
        write: creator_pool,
        write_requested: Arc::new(AtomicBool::new(false)),
    });
    let mut creator = tokio::spawn(async move {
        creator_manager
            .create_step(CreateStepInput {
                id: Some(retained_step_id.0),
                request_id: None,
                prev_step_id: None,
                parent_step_id: None,
                step_kind: StepKind::ToolCall,
                step_sequence: 101,
                request_payload: json!({"must_not": "survive-the-route-race"}),
            })
            .await
    });
    tokio::select! {
        result = &mut creator => panic!("same-ID creator did not block on movement: {result:?}"),
        () = wait_for_backend_lock_waiter(&pool, creator_pid) => {}
    }

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(MOVEMENT_AFTER_DELETE_GATE)
        .fetch_one(&mut *movement_gate)
        .await
        .unwrap();
    assert!(unlocked);
    let outcome = mover.await.unwrap().unwrap();
    assert_eq!(outcome.groups_archived, 1);

    let error = creator
        .await
        .unwrap()
        .expect_err("the successful same-ID INSERT must be rolled back after route recheck");
    assert_eq!(error.to_string(), "Response step is already retained");
    match &error {
        fusillade_arsenal::error::FusilladeError::Other(source) => assert!(
            source
                .downcast_ref::<RetainedResponseStepConflict>()
                .is_some()
        ),
        other => panic!("unexpected retained mutation error: {other:?}"),
    }
    assert_eq!(
        count_ids(&pool, "response_steps", &[retained_step_id.0]).await,
        0
    );
}

#[sqlx::test]
async fn movement_starting_after_create_insert_cannot_split_the_live_graph(pool: PgPool) {
    const CREATE_AFTER_INSERT_GATE: i64 = 730_017;

    install_candidate_index(&pool).await;
    ensure_partition(&pool, archive_date("2026-08-07")).await;
    let graph = branching_graph(&pool, TerminalState::Completed).await;
    let head_step_id = StepId(graph.group_id);
    let new_step_id = Uuid::new_v4();

    sqlx::query(
        r#"
        CREATE FUNCTION test_gate_after_response_step_insert()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            PERFORM pg_advisory_xact_lock(730017);
            RETURN NULL;
        END
        $$
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TRIGGER test_gate_after_response_step_insert
        AFTER INSERT ON response_steps
        FOR EACH STATEMENT
        EXECUTE FUNCTION test_gate_after_response_step_insert()
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut create_gate = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(CREATE_AFTER_INSERT_GATE)
        .execute(&mut *create_gate)
        .await
        .unwrap();
    let (_, creator_manager) = managers(&pool).await;
    let mut creator = tokio::spawn(async move {
        creator_manager
            .create_step(CreateStepInput {
                id: Some(new_step_id),
                request_id: None,
                prev_step_id: Some(head_step_id),
                parent_step_id: Some(head_step_id),
                step_kind: StepKind::ToolCall,
                step_sequence: 103,
                request_payload: json!({"input": "post-insert-boundary"}),
            })
            .await
    });
    tokio::select! {
        result = &mut creator => panic!("creator completed before the after-insert gate: {result:?}"),
        () = wait_for_advisory_waiter_key(&pool, CREATE_AFTER_INSERT_GATE) => {}
    }

    // The INSERT holds foreign-key protection on its live predecessor until
    // commit. A mover that starts now must skip the graph instead of moving
    // the old topology without the uncommitted child.
    let mover_manager = manager(&pool).await;
    let outcome = archive(
        &mover_manager,
        &policy(&[("flex", 86_400), ("priority", 3 * 86_400)]),
        1,
        i64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(outcome.groups_archived, 0);
    assert!(outcome.skipped_locked);
    assert!(outcome.may_have_more);

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(CREATE_AFTER_INSERT_GATE)
        .fetch_one(&mut *create_gate)
        .await
        .unwrap();
    assert!(unlocked);
    assert_eq!(creator.await.unwrap().unwrap(), StepId(new_step_id));
    assert_eq!(count_ids(&pool, "requests", &graph.request_ids).await, 2);
    assert_eq!(count_ids(&pool, "response_steps", &graph.step_ids).await, 5);
    assert_eq!(count_ids(&pool, "response_steps", &[new_step_id]).await, 1);
    assert_eq!(retained_counts(&pool, graph.group_id).await, (0, 0, 0));
}

#[sqlx::test]
async fn create_step_sanitizes_unrelated_foreign_key_error_without_active_route(pool: PgPool) {
    let (_, step_manager) = managers(&pool).await;
    let proposed_id = Uuid::new_v4();
    let missing_predecessor = StepId(Uuid::new_v4());

    let error = step_manager
        .create_step(CreateStepInput {
            id: Some(proposed_id),
            request_id: None,
            prev_step_id: Some(missing_predecessor),
            parent_step_id: None,
            step_kind: StepKind::ToolCall,
            step_sequence: 102,
            request_payload: json!({"input": "unrelated-fk-error"}),
        })
        .await
        .expect_err("an unrelated missing predecessor must remain a database error");

    assert_eq!(error.to_string(), "Failed to insert response step");
    let rendered = format!("{error:?}");
    assert!(!rendered.contains(&missing_predecessor.0.to_string()));
    assert!(!rendered.contains("unrelated-fk-error"));
    match &error {
        fusillade_arsenal::error::FusilladeError::Other(source) => assert!(
            source
                .downcast_ref::<RetainedResponseStepConflict>()
                .is_none()
        ),
        other => panic!("unexpected response-step insert error: {other:?}"),
    }
    assert_eq!(count_ids(&pool, "response_steps", &[proposed_id]).await, 0);
}
