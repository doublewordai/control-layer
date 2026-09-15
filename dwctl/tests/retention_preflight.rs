use sqlx::PgPool;

const QUERY: &str = include_str!("../src/retention_preflight.sql");

#[sqlx::test(migrations = false)]
async fn retired_route_probes_use_indexes_and_detect_either_route(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("fixtures/retention_preflight.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();

    // Match the production failure: many routes on live dates, none on a
    // retired date. EXISTS previously chose full scans to prove absence.
    let plan: serde_json::Value = sqlx::query_scalar(&format!("EXPLAIN (FORMAT JSON) {QUERY}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    fn check_route_scans(value: &serde_json::Value, scans: &mut usize) {
        match value {
            serde_json::Value::Object(object) => {
                if object
                    .get("Relation Name")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| matches!(name, "retained_response_group_routes" | "retained_response_request_routes"))
                {
                    *scans += 1;
                    assert!(
                        matches!(object["Node Type"].as_str(), Some("Index Scan" | "Index Only Scan")),
                        "retired-route probes must use indexes: {value}"
                    );
                    assert!(
                        object
                            .get("Index Cond")
                            .and_then(|v| v.as_str())
                            .is_some_and(|condition| condition.contains("delete_on")),
                        "index probes must be bounded by the bucket date: {value}"
                    );
                }
                for child in object.values() {
                    check_route_scans(child, scans);
                }
            }
            serde_json::Value::Array(children) => {
                for child in children {
                    check_route_scans(child, scans);
                }
            }
            _ => {}
        }
    }
    let mut scans = 0;
    check_route_scans(&plan, &mut scans);
    assert_eq!(scans, 2, "both route tables must be covered by the plan assertion");

    let (_, _, routes): (i64, bool, bool) = sqlx::query_as(QUERY).fetch_one(&mut *tx).await.unwrap();
    assert!(!routes, "routes belonging to active dates do not require retired-route cleanup");

    // Each kind of late route must independently keep recovery enabled.
    for (table, key) in [
        ("retained_response_group_routes", "group_id"),
        ("retained_response_request_routes", "request_id"),
    ] {
        sqlx::query(&format!(
            "INSERT INTO {table} ({key}, delete_on) VALUES ('00000000-0000-0000-0000-000000000001', '2026-09-01')"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        let (_, _, routes): (i64, bool, bool) = sqlx::query_as(QUERY).fetch_one(&mut *tx).await.unwrap();
        assert!(routes, "late routes in {table} must be detected");
        sqlx::query(&format!("DELETE FROM {table} WHERE delete_on = '2026-09-01'"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let (_, _, routes): (i64, bool, bool) = sqlx::query_as(QUERY).fetch_one(&mut *tx).await.unwrap();
        assert!(!routes, "cleaning {table} clears the recovery requirement");
    }
}
