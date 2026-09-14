//! Compare accepted durable completions against queue events and permanent receipts.
//! The bounded scan cycles by UUID, so a newly visible earlier UUID is revisited.
//! No billing transaction is held while reading the independent acceptance database.

use std::collections::{HashMap, HashSet};

use sqlx::FromRow;
use sqlx_pool_router::DynPools;
use uuid::Uuid;

#[derive(FromRow)]
struct Accepted {
    id: Uuid,
    accepted_event_id: Uuid,
}

/// Test convenience for deployments with an unknown analytics retention horizon.
#[cfg(test)]
pub(crate) async fn reconcile(main: &DynPools, fusillade: &DynPools, batch_size: usize) -> Result<(), sqlx::Error> {
    reconcile_with_retention(main, fusillade, batch_size, None).await
}

/// Supply the externally configured raw analytics retention horizon when known.
pub(crate) async fn reconcile_with_retention(
    main: &DynPools,
    fusillade: &DynPools,
    batch_size: usize,
    analytics_retention_days: Option<u32>,
) -> Result<(), sqlx::Error> {
    audit_receipts(main, fusillade, batch_size, analytics_retention_days).await?;
    let limit = batch_size.clamp(1, 100) as i64;
    let (cursor, revision): (Option<Uuid>, i64) =
        sqlx::query_as("SELECT last_request_id, revision FROM billing_reconciliation_cursor WHERE singleton")
            .fetch_one(&*main.write())
            .await?;

    // Every durable completion creates this compact proof atomically. Discovery
    // never scans legacy live/archive heaps, even when no durable work exists.
    let accepted = sqlx::query_as::<_, Accepted>(
        "SELECT request_id AS id,accepted_event_id FROM billing_acceptances
         WHERE completed_at < now()-interval '120 seconds'
           AND ($1::uuid IS NULL OR request_id>$1)
         ORDER BY request_id LIMIT $2",
    )
    .bind(cursor)
    .bind(limit)
    .fetch_all(&*fusillade.write())
    .await?;
    let ids: Vec<_> = accepted.iter().map(|request| request.id).collect();
    let event_ids: Vec<_> = accepted.iter().map(|request| request.accepted_event_id).collect();
    let receipts: HashSet<Uuid> = sqlx::query_scalar("SELECT request_id FROM billing_receipts WHERE request_id=ANY($1)")
        .bind(&ids)
        .fetch_all(&*main.write())
        .await?
        .into_iter()
        .collect();
    let events: HashMap<Uuid, (Uuid, String)> = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "SELECT event_id,request_id,processing_state FROM fusillade_billing_events WHERE event_id=ANY($1)",
    )
    .bind(&event_ids)
    .fetch_all(&*main.write())
    .await?
    .into_iter()
    .map(|(event, request, state)| (event, (request, state)))
    .collect();

    for request in &accepted {
        if receipts.contains(&request.id) {
            continue;
        }
        let reason = match events.get(&request.accepted_event_id) {
            Some((id, state)) if *id == request.id && state != "processed" => continue,
            Some((id, _)) if *id == request.id => "processed_without_receipt",
            _ => "missing_capture",
        };
        // Recheck the receipt at write time. Publication/billing may have advanced
        // since the bulk snapshot. A later receipt resolves evidence next tick.
        let inserted = sqlx::query(
            "INSERT INTO billing_reconciliation_issues(request_id,accepted_event_id,reason)
             SELECT $1,$2,$3 WHERE NOT EXISTS(SELECT 1 FROM billing_receipts WHERE request_id=$1)
             ON CONFLICT(request_id) DO NOTHING",
        )
        .bind(request.id)
        .bind(request.accepted_event_id)
        .bind(reason)
        .execute(&*main.write())
        .await?;
        if inserted.rows_affected() != 0 {
            crate::background_error!(
                crate::metrics::errors::component::ANALYTICS, "billing_reconciliation", Error,
                request_id = %request.id,
                accepted_event_id = ?request.accepted_event_id,
                reason,
                "Accepted durable completion requires billing reconciliation"
            );
        }
    }

    // Also resolve issues whose accepted request has since been archived away or
    // removed. Receipts outlive request retention and remain authoritative.
    sqlx::query(
        "UPDATE billing_reconciliation_issues SET resolved_at=now(),last_seen_at=now()
         WHERE request_id IN (
           SELECT issue.request_id FROM billing_reconciliation_issues issue
           JOIN billing_receipts receipt USING(request_id)
           WHERE issue.resolved_at IS NULL
             AND NOT EXISTS(SELECT 1 FROM billing_integrity_issues integrity WHERE integrity.request_id=issue.request_id AND integrity.resolved_at IS NULL)
             ORDER BY issue.request_id LIMIT $1
         )",
    )
    .bind(limit)
    .execute(&*main.write())
    .await?;

    let cycle_done = accepted.len() < limit as usize;
    let next_cursor = if cycle_done {
        None
    } else {
        accepted.last().map(|request| request.id)
    };
    // Another worker may have advanced the same page. Revision comparison also
    // prevents an old end-of-cycle observer from resetting a newer scan.
    sqlx::query(
        "UPDATE billing_reconciliation_cursor
         SET last_request_id=$1, revision=revision+1,last_audit_at=now(),
             completed_cycles=completed_cycles+CASE WHEN $2 THEN 1 ELSE 0 END,
             last_cycle_completed_at=CASE WHEN $2 THEN now() ELSE last_cycle_completed_at END
         WHERE singleton AND revision=$3",
    )
    .bind(next_cursor)
    .bind(cycle_done)
    .bind(revision)
    .execute(&*main.write())
    .await?;
    let (issues, cycles, age): (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM billing_reconciliation_issues WHERE resolved_at IS NULL),
                completed_cycles,EXTRACT(EPOCH FROM now()-last_cycle_completed_at)::float8
         FROM billing_reconciliation_cursor WHERE singleton",
    )
    .fetch_one(&*main.write())
    .await?;
    metrics::gauge!("dwctl_billing_reconciliation_issues").set(issues as f64);
    metrics::gauge!("dwctl_billing_reconciliation_completed_cycles").set(cycles as f64);
    if let Some(age) = age {
        metrics::gauge!("dwctl_billing_reconciliation_cycle_age_seconds").set(age.max(0.0));
    }
    Ok(())
}

#[derive(FromRow)]
struct ReceiptAudit {
    request_id: Uuid,
    event_id: Uuid,
    owner_id: Uuid,
    source_ok: bool,
    ledger_ok: bool,
    analytics_ok: bool,
    analytics_unverifiable: bool,
    event_ok: bool,
    batch_ok: bool,
}

#[derive(FromRow)]
struct ReceiptAcceptance {
    id: Uuid,
    accepted_event_id: Option<Uuid>,
    created_by: Option<String>,
    state: String,
    billing_mode: Option<String>,
}

// Shared aggregate rows have no per-receipt contribution identity. Their checks
// below establish presence and lower bounds, not exact whole-batch parity.
async fn audit_receipts(
    main: &DynPools,
    fusillade: &DynPools,
    batch_size: usize,
    analytics_retention_days: Option<u32>,
) -> Result<(), sqlx::Error> {
    let limit = batch_size.clamp(1, 100) as i64;
    let (cursor, revision): (Option<Uuid>, i64) =
        sqlx::query_as("SELECT last_receipt_id,receipt_revision FROM billing_reconciliation_cursor WHERE singleton")
            .fetch_one(&*main.write())
            .await?;
    // A single primary statement observes receipt, ledger and projections in the
    // same MVCC snapshot. In-progress worker transactions cannot cause false drift.
    let receipts = sqlx::query_as::<_, ReceiptAudit>(
        r#"
      WITH page AS (
        SELECT request_id, event_id, owner_id, ledger_source_id, total_cost, analytics_timestamp,
          uncached_cost, input_price_per_token, output_price_per_token, analytics_id
        FROM billing_receipts WHERE ($1::uuid IS NULL OR request_id>$1)
        ORDER BY request_id LIMIT $2
      )
      SELECT r.request_id,r.event_id,r.owner_id,
        r.ledger_source_id='durable-billing:'||r.request_id::text AS source_ok,
        CASE WHEN r.total_cost=0 THEN ledger.n=0 ELSE ledger.n=1 AND ledger.valid=1 END AS ledger_ok,
        CASE WHEN a.id IS NULL THEN analytics_count.n=0 AND NOT COALESCE(
          $3::int IS NOT NULL AND r.analytics_timestamp >= now()-make_interval(days=>$3),false)
        ELSE COALESCE(a.fusillade_request_id=r.request_id AND a.user_id=r.owner_id
          AND a.total_cost=r.total_cost AND a.uncached_cost=r.uncached_cost
          AND a.input_price_per_token=r.input_price_per_token AND a.output_price_per_token=r.output_price_per_token
          AND analytics_count.n=1, false) END AS analytics_ok,
        a.id IS NULL AND NOT COALESCE(
          $3::int IS NOT NULL AND r.analytics_timestamp >= now()-make_interval(days=>$3),false)
          AND analytics_count.n=0 AS analytics_unverifiable,
        CASE WHEN e.event_id IS NULL THEN true ELSE COALESCE(
          e.request_id=r.request_id AND e.owner_id=r.owner_id AND e.billing_mode='durable'
          AND e.processing_state='processed' AND e.disposition='billed'
          AND (a.id IS NULL OR (a.prompt_tokens=e.prompt_tokens AND a.completion_tokens=e.completion_tokens
          AND a.total_tokens=COALESCE(e.total_tokens,LEAST(9223372036854775807::numeric,e.prompt_tokens::numeric+e.completion_tokens::numeric)::bigint)
          AND a.reasoning_tokens=COALESCE(e.reasoning_tokens,0)
          AND a.cache_read_input_tokens=COALESCE(e.cache_read_input_tokens,0)
          AND a.cache_creation_5m_input_tokens=COALESCE(e.cache_creation_5m_input_tokens,0)
          AND a.cache_creation_1h_input_tokens=COALESCE(e.cache_creation_1h_input_tokens,0)
          AND a.cache_creation_24h_input_tokens=COALESCE(e.cache_creation_24h_input_tokens,0)
          AND a.engine_cached_tokens IS NOT DISTINCT FROM e.engine_cached_tokens
          AND a.fusillade_batch_id IS NOT DISTINCT FROM e.batch_id)), false) END AS event_ok,
        CASE WHEN a.fusillade_batch_id IS NULL THEN true ELSE COALESCE(
          b.user_id=r.owner_id AND b.total_requests>=1 AND b.total_amount>=r.total_cost
          AND b.total_prompt_tokens>=a.prompt_tokens AND b.total_completion_tokens>=a.completion_tokens
          AND b.total_tokens>=a.total_tokens AND b.total_list_cost>=r.uncached_cost
          AND (r.total_cost=0 OR b.transaction_count>=1), false) END AS batch_ok
      FROM page r
      LEFT JOIN http_analytics a ON a.id=r.analytics_id
      LEFT JOIN fusillade_billing_events e ON e.event_id=r.event_id
      LEFT JOIN batch_aggregates b ON b.fusillade_batch_id=a.fusillade_batch_id
      LEFT JOIN LATERAL (
        SELECT count(*) AS n,count(*) FILTER(WHERE t.source_id=r.ledger_source_id
          AND t.user_id=r.owner_id AND t.fusillade_request_id=r.request_id
          AND t.transaction_type='usage' AND t.amount=r.total_cost) AS valid
        FROM credits_transactions t WHERE t.source_id=r.ledger_source_id OR t.fusillade_request_id=r.request_id
      ) ledger ON true
      LEFT JOIN LATERAL (
        SELECT count(*) AS n FROM http_analytics projected WHERE projected.fusillade_request_id=r.request_id
      ) analytics_count ON true
      ORDER BY r.request_id
    "#,
    )
    .bind(cursor)
    .bind(limit)
    .bind(analytics_retention_days.map(|days| days.min(i32::MAX as u32) as i32))
    .fetch_all(&*main.write())
    .await?;
    let unverifiable = receipts.iter().filter(|r| r.analytics_unverifiable).count();
    metrics::gauge!("dwctl_billing_receipt_page_analytics_unverifiable").set(unverifiable as f64);
    let ids: Vec<_> = receipts.iter().map(|r| r.request_id).collect();
    // Compact canonical acceptance survives payload retention and takes precedence
    // over live/archive rows. The fallback supports receipts created before the
    // canonical table was introduced; their payload-derived ownership may expire.
    let accepted: HashMap<Uuid, ReceiptAcceptance> = sqlx::query_as::<_, ReceiptAcceptance>(
        "SELECT request_id AS id,accepted_event_id,owner_id AS created_by,'completed'::text AS state,'durable'::text AS billing_mode
         FROM billing_acceptances WHERE request_id=ANY($1)
         UNION ALL
         SELECT r.id,r.accepted_event_id,COALESCE(NULLIF(r.created_by,''),b.created_by),r.state,r.billing_mode
         FROM requests r LEFT JOIN batches b ON b.id=r.batch_id WHERE r.id=ANY($1)
           AND NOT EXISTS(SELECT 1 FROM billing_acceptances c WHERE c.request_id=r.id)
         UNION ALL
         SELECT r.id,r.accepted_event_id,COALESCE(NULLIF(r.created_by,''),b.created_by),r.state,r.billing_mode
         FROM batch_requests_archive r LEFT JOIN batches b ON b.id=r.batch_id WHERE r.id=ANY($1)
           AND NOT EXISTS(SELECT 1 FROM billing_acceptances c WHERE c.request_id=r.id)",
    )
    .bind(&ids)
    .fetch_all(&*fusillade.write())
    .await?
    .into_iter()
    .map(|a| (a.id, a))
    .collect();
    for receipt in &receipts {
        let mut reasons = Vec::new();
        for (valid, reason) in [
            (receipt.source_ok, "receipt_source_mismatch"),
            (receipt.ledger_ok, "receipt_ledger_mismatch"),
            (receipt.analytics_ok, "receipt_analytics_mismatch"),
            (receipt.event_ok, "receipt_event_mismatch"),
            (receipt.batch_ok, "receipt_batch_projection_mismatch"),
        ] {
            if !valid {
                reasons.push(reason);
            }
        }
        if let Some(acceptance) = accepted.get(&receipt.request_id) {
            if acceptance.accepted_event_id != Some(receipt.event_id)
                || acceptance.state != "completed"
                || acceptance.billing_mode.as_deref() != Some("durable")
            {
                reasons.push("receipt_acceptance_mismatch");
            }
            if acceptance.created_by.as_deref().and_then(|owner| owner.parse::<Uuid>().ok()) != Some(receipt.owner_id) {
                reasons.push("receipt_owner_mismatch");
            }
        }
        for reason in &reasons {
            let inserted = sqlx::query(
                "INSERT INTO billing_integrity_issues(request_id,event_id,reason) VALUES($1,$2,$3)
                 ON CONFLICT(request_id,reason) DO NOTHING",
            )
            .bind(receipt.request_id)
            .bind(receipt.event_id)
            .bind(reason)
            .execute(&*main.write())
            .await?;
            if inserted.rows_affected() != 0 {
                crate::background_error!(crate::metrics::errors::component::ANALYTICS, "billing_integrity", Error,
                    request_id=%receipt.request_id,event_id=%receipt.event_id,reason=*reason,
                    "Durable billing receipt integrity check failed");
            }
        }
        // Retain history; reopen a previously resolved issue if drift recurs.
        sqlx::query("UPDATE billing_integrity_issues SET last_seen_at=now(),resolved_at=CASE WHEN reason=ANY($2) THEN NULL ELSE COALESCE(resolved_at,now()) END WHERE request_id=$1")
            .bind(receipt.request_id).bind(&reasons).execute(&*main.write()).await?;
    }
    let done = receipts.len() < limit as usize;
    let next = if done { None } else { receipts.last().map(|r| r.request_id) };
    sqlx::query("UPDATE billing_reconciliation_cursor SET last_receipt_id=$1,receipt_revision=receipt_revision+1,last_receipt_audit_at=now(),last_receipt_cycle_completed_at=CASE WHEN $2 THEN now() ELSE last_receipt_cycle_completed_at END WHERE singleton AND receipt_revision=$3")
        .bind(next).bind(done).bind(revision).execute(&*main.write()).await?;
    let issues: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
        .fetch_one(&*main.write())
        .await?;
    metrics::gauge!("dwctl_billing_integrity_issues").set(issues as f64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Executor, PgPool};

    async fn acceptance_pool(main: &PgPool) -> PgPool {
        main.execute("CREATE SCHEMA acceptance_audit_test").await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                main.connect_options()
                    .as_ref()
                    .clone()
                    .options([("search_path", "acceptance_audit_test")]),
            )
            .await
            .unwrap();
        pool.execute(
            "CREATE TABLE requests(id uuid PRIMARY KEY,accepted_event_id uuid,billing_mode text,state text,completed_at timestamptz,created_by text,batch_id uuid)",
        )
        .await
        .unwrap();
        pool.execute("CREATE TABLE billing_acceptances(request_id uuid PRIMARY KEY,accepted_event_id uuid NOT NULL,completed_at timestamptz NOT NULL,owner_id text,batch_id uuid)")
            .await.unwrap();
        pool.execute("CREATE TABLE batches(id uuid PRIMARY KEY,created_by text)")
            .await
            .unwrap();
        pool.execute("CREATE TABLE batch_requests_archive (LIKE requests INCLUDING ALL)")
            .await
            .unwrap();
        pool
    }

    async fn accept(pool: &PgPool, id: Uuid, event: Uuid, archive: bool) {
        let statement = if archive {
            "INSERT INTO batch_requests_archive (id,accepted_event_id,billing_mode,state,completed_at) VALUES($1,$2,'durable','completed',now()-interval '3 minutes')"
        } else {
            "INSERT INTO requests (id,accepted_event_id,billing_mode,state,completed_at) VALUES($1,$2,'durable','completed',now()-interval '3 minutes')"
        };
        sqlx::query(statement).bind(id).bind(event).execute(pool).await.unwrap();
        // Mirror the production completion trigger's permanent acceptance proof.
        sqlx::query("INSERT INTO billing_acceptances(request_id,accepted_event_id,completed_at) VALUES($1,$2,now()-interval '3 minutes') ON CONFLICT(request_id) DO NOTHING")
            .bind(id).bind(event).execute(pool).await.unwrap();
    }

    async fn receipt(pool: &PgPool, id: Uuid, event: Uuid) -> Uuid {
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,username,email) VALUES($1,$2,$3)")
            .bind(owner)
            .bind(owner.to_string())
            .bind(format!("{owner}@example.test"))
            .execute(pool)
            .await
            .unwrap();
        let analytics:i64=sqlx::query_scalar("INSERT INTO http_analytics(instance_id,correlation_id,timestamp,method,uri,user_id,fusillade_request_id,total_cost,uncached_cost,input_price_per_token,output_price_per_token) VALUES($1,1,now(),'POST','/v1/chat/completions',$2,$3,0,0,0,0) RETURNING id")
            .bind(Uuid::new_v4()).bind(owner).bind(id).fetch_one(pool).await.unwrap();
        sqlx::query("INSERT INTO billing_receipts(request_id,owner_id,event_id,total_cost,ledger_source_id,analytics_id,input_price_per_token,output_price_per_token,uncached_cost,analytics_timestamp) VALUES($1,$2,$3,0,$4,$5,0,0,0,now())")
            .bind(id).bind(owner).bind(event).bind(format!("durable-billing:{id}")).bind(analytics).execute(pool).await.unwrap();
        owner
    }

    #[sqlx::test]
    async fn audit_cycles_live_and_archive_and_resolves_only_after_receipt(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance.clone());
        let (first, second, earlier) = (Uuid::from_u128(20), Uuid::from_u128(30), Uuid::from_u128(10));
        let event = Uuid::new_v4();
        accept(&acceptance, first, event, false).await;
        accept(&acceptance, second, Uuid::new_v4(), true).await;
        reconcile(&main, &fusillade, 1).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_reconciliation_issues")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "each call inspects a bounded acceptance page");
        accept(&acceptance, earlier, Uuid::new_v4(), false).await;
        reconcile(&main, &fusillade, 1).await.unwrap();
        reconcile(&main, &fusillade, 1).await.unwrap(); // end resets cursor
        reconcile(&main, &fusillade, 1).await.unwrap(); // earlier UUID next cycle
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_reconciliation_issues")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
        let owner = receipt(&pool, first, event).await;
        sqlx::query("UPDATE billing_acceptances SET owner_id=$2 WHERE request_id=$1")
            .bind(first)
            .bind(owner.to_string())
            .execute(&acceptance)
            .await
            .unwrap();
        reconcile(&main, &fusillade, 1).await.unwrap();
        let resolved: bool = sqlx::query_scalar("SELECT resolved_at IS NOT NULL FROM billing_reconciliation_issues WHERE request_id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(resolved);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_reconciliation_issues")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3, "issue history remains retained and deduplicated");
    }

    #[sqlx::test]
    async fn audit_distinguishes_pending_capture_processed_without_receipt_and_grace(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance.clone());
        let id = Uuid::new_v4();
        let event = Uuid::new_v4();
        accept(&acceptance, id, event, false).await;
        sqlx::query("INSERT INTO fusillade_billing_events(event_id,request_id,retry_attempt,owner_id,requested_model,started_at,completed_at,usage_present,billing_mode) VALUES($1,$2,0,$3,'model',now(),now(),true,'durable')")
            .bind(event).bind(id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_reconciliation_issues")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "pending events belong to the worker backlog");
        sqlx::query("UPDATE fusillade_billing_events SET processing_state='processed',disposition='unaccepted_attempt' WHERE event_id=$1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let reason: String = sqlx::query_scalar("SELECT reason FROM billing_reconciliation_issues WHERE request_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            reason, "processed_without_receipt",
            "an accepted event cannot be disposed of as an unused attempt"
        );
        let recent = Uuid::new_v4();
        sqlx::query("INSERT INTO billing_acceptances(request_id,accepted_event_id,completed_at) VALUES($1,$2,now())")
            .bind(recent)
            .bind(Uuid::new_v4())
            .execute(&acceptance)
            .await
            .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_reconciliation_issues WHERE request_id=$1")
            .bind(recent)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "recent acceptance receives a publication/processing grace period");
    }
    #[sqlx::test]
    async fn receipt_audit_survives_retention_and_rejects_zero_cost_debit(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance);
        let id = Uuid::new_v4();
        let owner = receipt(&pool, id, Uuid::new_v4()).await;
        // No acceptance or event retained: permanent receipt and projections audit independently.
        reconcile(&main, &fusillade, 100).await.unwrap();
        let issues: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(issues, 0);
        sqlx::query(
            "INSERT INTO credits_transactions(user_id,transaction_type,amount,source_id,fusillade_request_id) VALUES($1,'usage',1,$2,$3)",
        )
        .bind(owner)
        .bind(format!("durable-billing:{id}"))
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let ledger_issue:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_integrity_issues WHERE request_id=$1 AND reason='receipt_ledger_mismatch' AND resolved_at IS NULL)")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert!(ledger_issue, "a zero-cost receipt must have no debit");
    }

    #[sqlx::test]
    async fn receipt_audit_detects_identity_ledger_and_projection_drift(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance.clone());
        let id = Uuid::new_v4();
        let event = Uuid::new_v4();
        let owner = receipt(&pool, id, event).await;
        accept(&acceptance, id, Uuid::new_v4(), true).await;
        sqlx::query("UPDATE billing_acceptances SET owner_id=$2 WHERE request_id=$1")
            .bind(id)
            .bind(Uuid::new_v4().to_string())
            .execute(&acceptance)
            .await
            .unwrap();
        sqlx::query("UPDATE billing_receipts SET total_cost=1 WHERE request_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let reasons: Vec<String> =
            sqlx::query_scalar("SELECT reason FROM billing_integrity_issues WHERE request_id=$1 AND resolved_at IS NULL ORDER BY reason")
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            reasons,
            vec![
                "receipt_acceptance_mismatch",
                "receipt_analytics_mismatch",
                "receipt_ledger_mismatch",
                "receipt_owner_mismatch"
            ]
        );
        // Repair only authoritative test evidence; the reconciler performs no repairs.
        sqlx::query("UPDATE billing_acceptances SET owner_id=$2,accepted_event_id=$3 WHERE request_id=$1")
            .bind(id)
            .bind(owner.to_string())
            .bind(event)
            .execute(&acceptance)
            .await
            .unwrap();
        sqlx::query("UPDATE http_analytics SET total_cost=1 WHERE fusillade_request_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO credits_transactions(user_id,transaction_type,amount,source_id,fusillade_request_id) VALUES($1,'usage',1,$2,$3)",
        )
        .bind(owner)
        .bind(format!("durable-billing:{id}"))
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let open: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE request_id=$1 AND resolved_at IS NULL")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open, 0, "verified repairs resolve issues while retaining their history");
    }
    #[sqlx::test]
    async fn receipt_audit_respects_analytics_retention_and_total_token_fallback(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance);
        let id = Uuid::new_v4();
        let event = Uuid::new_v4();
        let owner = receipt(&pool, id, event).await;
        sqlx::query(
            "UPDATE http_analytics SET prompt_tokens=3,completion_tokens=2,total_tokens=5,reasoning_tokens=0 WHERE fusillade_request_id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO fusillade_billing_events(event_id,request_id,retry_attempt,owner_id,requested_model,started_at,completed_at,usage_present,billing_mode,prompt_tokens,completion_tokens,processing_state,disposition) VALUES($1,$2,0,$3,'model',now(),now(),true,'durable',3,2,'processed','billed')")
            .bind(event).bind(id).bind(owner).execute(&pool).await.unwrap();
        reconcile_with_retention(&main, &fusillade, 100, Some(35)).await.unwrap();
        let open: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open, 0, "NULL provider total uses the same prompt+completion fallback as billing");
        sqlx::query("DELETE FROM http_analytics WHERE fusillade_request_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        reconcile_with_retention(&main, &fusillade, 100, Some(35)).await.unwrap();
        let reasons: Vec<String> = sqlx::query_scalar("SELECT reason FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            reasons,
            vec!["receipt_analytics_mismatch"],
            "missing analytics inside a known retention horizon is actionable"
        );
        sqlx::query("UPDATE billing_receipts SET analytics_timestamp=now()-interval '40 days' WHERE request_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        reconcile_with_retention(&main, &fusillade, 100, Some(35)).await.unwrap();
        let open: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            open, 0,
            "expired raw analytics is unverifiable, not a permanent corruption alert; retained event identity still audits"
        );
    }
    #[sqlx::test]
    async fn canonical_acceptance_survives_payload_retention_and_takes_precedence(pool: PgPool) {
        let acceptance = acceptance_pool(&pool).await;
        let main = DynPools::new(pool.clone());
        let fusillade = DynPools::new(acceptance.clone());
        let id = Uuid::new_v4();
        let event = Uuid::new_v4();
        let owner = receipt(&pool, id, event).await;
        let missing = Uuid::new_v4();
        for (request_id, accepted_event) in [(id, event), (missing, Uuid::new_v4())] {
            sqlx::query("INSERT INTO billing_acceptances(request_id,accepted_event_id,completed_at,owner_id) VALUES($1,$2,now()-interval '3 minutes',$3)")
                .bind(request_id).bind(accepted_event).bind(owner.to_string()).execute(&acceptance).await.unwrap();
        }
        // Even a conflicting payload projection cannot replace canonical evidence.
        accept(&acceptance, id, Uuid::new_v4(), true).await;
        reconcile(&main, &fusillade, 100).await.unwrap();
        let integrity: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(integrity, 0, "canonical event/owner take precedence over stale archive data");
        let missing_capture: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM billing_reconciliation_issues WHERE request_id=$1 AND reason='missing_capture')",
        )
        .bind(missing)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(missing_capture, "canonical acceptance without any payload row remains discoverable");
        acceptance.execute("DELETE FROM batch_requests_archive").await.unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let integrity: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_integrity_issues WHERE resolved_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(integrity, 0);
        sqlx::query("UPDATE billing_receipts SET event_id=$2 WHERE request_id=$1")
            .bind(id)
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await
            .unwrap();
        reconcile(&main, &fusillade, 100).await.unwrap();
        let mismatch:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_integrity_issues WHERE request_id=$1 AND reason='receipt_acceptance_mismatch' AND resolved_at IS NULL)")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert!(
            mismatch,
            "permanent acceptance identity still validates receipts after payload erasure"
        );
    }
}
