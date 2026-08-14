use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use fusillade_arsenal::manager::{
    RetainedResponseArchiveOutcome, RetainedResponseMaintenanceError, RetentionSweepPolicy,
};
use fusillade_arsenal::{
    DaemonStorage, PostgresRequestManager, PostgresStorageConfig, TestDbPools,
};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::Barrier;
use uuid::Uuid;

const MODEL: &str = "retention-test-model";
const OWNER: &str = "retention-test-owner";

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

fn timestamp(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp must be valid")
        .to_utc()
}

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("fixture date must be valid")
}

fn policy(tiers: &[(&str, u64)]) -> RetentionSweepPolicy {
    RetentionSweepPolicy {
        batchless_seconds_by_service_tier: tiers
            .iter()
            .map(|(tier, seconds)| ((*tier).to_owned(), *seconds))
            .collect::<HashMap<_, _>>(),
        ..Default::default()
    }
}

async fn manager(pool: &PgPool) -> PostgresRequestManager<TestDbPools> {
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone())
            .await
            .expect("test pools must initialize"),
        PostgresStorageConfig::default(),
    )
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
            state: second_request_state,
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

async fn archive(
    manager: &PostgresRequestManager<TestDbPools>,
    policy: &RetentionSweepPolicy,
    max_groups: i64,
    max_bytes: i64,
) -> fusillade_arsenal::error::Result<RetainedResponseArchiveOutcome> {
    manager
        .archive_terminal_batchless_responses(
            policy,
            timestamp("2026-08-09T00:00:00Z"),
            max_groups,
            max_bytes,
        )
        .await
}

#[sqlx::test]
async fn archives_singleton_request_template_as_one_group(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-07")).await;
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
    assert_eq!(delete_on, date("2026-08-07"));
}

#[sqlx::test]
async fn locked_candidate_is_skipped_without_loading_or_moving_graph(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-07")).await;
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
async fn dispatched_cancellation_obeys_the_absolute_grace_cutoff(pool: PgPool) {
    install_candidate_index(&pool).await;
    ensure_partition(&pool, date("2026-08-06")).await;
    ensure_partition(&pool, date("2026-08-13")).await;
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

    let outcome = archive(&manager, &policy(&[("flex", 1)]), 2, i64::MAX)
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-07")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
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
    ensure_partition(&pool, date("2026-08-03")).await;
    sqlx::query("UPDATE retained_response_buckets SET state = 'retiring' WHERE delete_on = $1")
        .bind(date("2026-08-03"))
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
    ensure_partition(&pool, date("2026-08-03")).await;
    ensure_partition(&pool, date("2026-08-04")).await;
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
        .bind(date("2026-08-04"))
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
