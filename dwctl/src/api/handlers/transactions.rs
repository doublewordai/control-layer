//! HTTP handlers for credit transaction endpoints.

use crate::{
    AppState,
    api::models::{
        transactions::{
            CreditTransactionCreate, CreditTransactionResponse, ListTransactionsQuery, TransactionFilters, TransactionListResponse,
        },
        users::CurrentUser,
    },
    auth::permissions::{self, RequiresPermission, operation, resource},
    db::{
        handlers::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    errors::{Error, Result},
    types::{Operation, Permission, Resource, UserId},
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};

use fusillade::Storage;
use rust_decimal::Decimal;
use std::collections::HashMap;
use uuid::Uuid;

/// Create a new credit transaction
#[utoipa::path(
    post,
    path = "/transactions",
    tag = "transactions",
    summary = "Create a credit transaction",
    description = "Create a new credit transaction to grant or remove credits (BillingManager role required)",
    request_body = CreditTransactionCreate,
    responses(
        (status = 201, description = "Transaction created successfully", body = CreditTransactionResponse),
        (status = 400, description = "Bad request - invalid transaction type or amount"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - requires BillingManager role"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_transaction(
    State(state): State<AppState>,
    _perm: RequiresPermission<resource::Credits, operation::CreateAll>,
    Json(data): Json<CreditTransactionCreate>,
) -> Result<(StatusCode, Json<CreditTransactionResponse>)> {
    // Validate amount is positive
    if data.amount <= Decimal::ZERO {
        return Err(Error::BadRequest {
            message: "Amount must be greater than zero".to_string(),
        });
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    // Create the transaction
    let db_request = CreditTransactionCreateDBRequest {
        user_id: data.user_id,
        transaction_type: CreditTransactionType::from(&data.transaction_type),
        amount: data.amount,
        source_id: data.source_id,
        description: data.description,
        fusillade_batch_id: None,
    };

    let transaction = repo.create_transaction(&db_request).await?;

    Ok((StatusCode::CREATED, Json(CreditTransactionResponse::from(transaction))))
}

/// Get a specific transaction by ID
#[utoipa::path(
    get,
    path = "/transactions/{transaction_id}",
    tag = "transactions",
    summary = "Get a specific transaction",
    description = "Get details of a specific credit transaction. Non-BillingManager users can only access their own transactions.",
    params(
        ("transaction_id" = i64, Path, description = "Transaction ID"),
    ),
    responses(
        (status = 200, description = "Transaction details", body = CreditTransactionResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Transaction not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_transaction(
    State(state): State<AppState>,
    Path(transaction_id): Path<Uuid>,
    current_user: CurrentUser,
) -> Result<Json<CreditTransactionResponse>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    // Check permissions: if not BillingManager and not admin, must be own transaction
    let has_read_all = permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll);

    let transaction = repo.get_transaction_by_id(transaction_id).await?;

    let transaction = match transaction {
        Some(tx) => {
            if !has_read_all && tx.user_id != current_user.id {
                // Return 404 to avoid leaking existence
                return Err(Error::NotFound {
                    resource: "Transaction".to_string(),
                    id: transaction_id.to_string(),
                });
            }
            tx
        }
        None => {
            return Err(Error::NotFound {
                resource: "Transaction".to_string(),
                id: transaction_id.to_string(),
            });
        }
    };

    Ok(Json(CreditTransactionResponse::from(transaction)))
}

/// List credit transactions
#[utoipa::path(
    get,
    path = "/transactions",
    tag = "transactions",
    summary = "List credit transactions",
    description = "Get a paginated list of credit transactions with balance context. By default, returns only the current user's transactions. Use 'all=true' to get all transactions (BillingManager/PlatformManager only). Use 'user_id' parameter to filter by a specific user (BillingManager/PlatformManager only for other users).",
    params(
        ListTransactionsQuery
    ),
    responses(
        (status = 200, description = "Paginated list of transactions with balance context", body = TransactionListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - cannot access other users' transactions or all transactions without proper permissions"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_transactions(
    State(state): State<AppState>,
    Query(query): Query<ListTransactionsQuery>,
    current_user: CurrentUser,
) -> Result<Json<TransactionListResponse>> {
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();

    // Check if user has ReadAll permission
    let has_read_all = permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll);

    // Check if requesting all transactions
    if query.all == Some(true) && !has_read_all {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Credits, Operation::ReadAll),
            action: Operation::ReadAll,
            resource: "all transactions".to_string(),
        });
    }

    // Determine which user_id to filter by
    let filter_user_id = match (query.all, query.user_id) {
        // all=true takes precedence - return all transactions
        (Some(true), _) => None,
        // user_id specified - filter to that user
        (_, Some(requested_user_id)) => {
            // If requesting specific user's transactions
            if !has_read_all && requested_user_id != current_user.id {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::Credits, Operation::ReadAll),
                    action: Operation::ReadAll,
                    resource: "transactions".to_string(),
                });
            }
            Some(requested_user_id)
        }
        // No parameters - default to current user
        (_, None) => Some(current_user.id),
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    // Parse filters from query
    let filters = query.to_filters();

    // Batch grouping only works with a user filter (requires per-user batch_aggregates table)
    let grouping_enabled = query.group_batches.unwrap_or(false) && filter_user_id.is_some();

    // Get transactions for this page
    let (transactions, total_count) = if let (true, Some(user_id)) = (grouping_enabled, filter_user_id) {
        let transactions_with_categories = repo.list_transactions_with_batches(user_id, skip, limit, &filters).await?;
        let count = repo.count_transactions_with_batches(user_id, &filters).await?;

        // Collect unique batch_ids that need SLA lookup from fusillade
        let batch_ids: Vec<Uuid> = transactions_with_categories.iter().filter_map(|twc| twc.batch_id).collect();

        // Fetch batch completion_window (SLA) from fusillade for each batch
        let mut batch_sla_map: HashMap<Uuid, String> = HashMap::new();
        for batch_id in batch_ids {
            if let Ok(batch) = state.request_manager.get_batch(fusillade::BatchId(batch_id)).await {
                batch_sla_map.insert(batch_id, batch.completion_window);
            }
        }

        let txs: Vec<CreditTransactionResponse> = transactions_with_categories
            .into_iter()
            .map(|twc| {
                // For batched transactions, get SLA from fusillade; for others, use what we got from http_analytics
                let batch_sla = if let Some(batch_id) = twc.batch_id {
                    batch_sla_map.get(&batch_id).cloned()
                } else {
                    twc.batch_sla
                };
                CreditTransactionResponse::from_db_with_category(
                    twc.transaction,
                    twc.batch_id,
                    twc.request_origin,
                    batch_sla,
                    twc.batch_count,
                )
            })
            .collect();
        (txs, count)
    } else if let Some(user_id) = filter_user_id {
        let txs = repo.list_user_transactions(user_id, skip, limit, &filters).await?;
        let count = repo.count_user_transactions(user_id, &filters).await?;
        (txs.into_iter().map(CreditTransactionResponse::from).collect(), count)
    } else {
        let txs = repo.list_all_transactions(skip, limit, &filters).await?;
        let count = repo.count_all_transactions(&filters).await?;
        (txs.into_iter().map(CreditTransactionResponse::from).collect(), count)
    };

    // Calculate page_start_balance
    // For single-user queries: current balance when skip=0, or balance at the pagination
    // point (before the first transaction on this page) when skip>0
    // For all-users queries (no filter), we return 0 since per-user balance doesn't make sense
    // When batch grouping is enabled, use the grouped sum to match the grouped transaction list
    // Filters are applied so that date-filtered views show the correct starting balance
    let page_start_balance = if let Some(user_id) = filter_user_id {
        calculate_page_start_balance(&mut repo, user_id, skip, grouping_enabled, &filters).await?
    } else {
        // When viewing all users' transactions, per-row balance doesn't apply
        Decimal::ZERO
    };

    Ok(Json(TransactionListResponse {
        data: transactions,
        total_count,
        skip,
        limit,
        page_start_balance,
    }))
}

/// Calculate the balance at the start of a page for pagination purposes.
/// - Returns the balance that should be shown after the first transaction on the page
/// - When end_date filter is set, calculates balance at that point in time
/// - When skip > 0, subtracts the skipped transactions from the starting balance
/// - If use_grouped=true: uses grouped transaction view (batch aggregates count as single items)
async fn calculate_page_start_balance(
    repo: &mut Credits<'_>,
    user_id: UserId,
    skip: i64,
    use_grouped: bool,
    filters: &TransactionFilters,
) -> Result<Decimal> {
    let current_balance = repo.get_user_balance(user_id).await?;

    // If there's an end_date filter, we need to calculate the balance at that point in time
    // by subtracting all transactions that occurred after the end_date
    let balance_at_filter_end = if let Some(end_date) = filters.end_date {
        let after_sum = if use_grouped {
            repo.sum_transactions_after_date_grouped(user_id, end_date).await?
        } else {
            repo.sum_transactions_after_date(user_id, end_date).await?
        };
        current_balance - after_sum
    } else {
        current_balance
    };

    if skip == 0 {
        return Ok(balance_at_filter_end);
    }

    // Sum the signed amounts of the first `skip` transactions (most recent ones we're skipping)
    // within the filtered set. When batch grouping is enabled, use the grouped sum so that
    // batch aggregates count as single items, matching the pagination of list_transactions_with_batches.
    let skipped_sum = if use_grouped {
        repo.sum_recent_transactions_grouped(user_id, skip, filters).await?
    } else {
        repo.sum_recent_transactions(user_id, skip, filters).await?
    };

    Ok(balance_at_filter_end - skipped_sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::Role,
        db::{handlers::Credits as CreditsHandler, models::credits::CreditTransactionCreateDBRequest},
        test::utils::*,
        types::UserId,
    };
    use rust_decimal::Decimal;
    use serde_json::json;
    use sqlx::PgPool;
    use std::str::FromStr;

    async fn create_initial_credit_transaction(pool: &PgPool, user_id: UserId, amount: &str) -> Uuid {
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = CreditsHandler::new(&mut conn);

        let amount_decimal = Decimal::from_str(amount).expect("Invalid decimal amount");
        let request =
            CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, amount_decimal, Some("Initial credit grant".to_string()));

        credits_repo
            .create_transaction(&request)
            .await
            .expect("Failed to create transaction")
            .id
    }

    // Test: BillingManager can create transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_create_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "source_id": user.id.to_string(),
            "description": "Test credit grant"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("100.0").unwrap());
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.source_id, user.id.to_string());
        assert_eq!(transaction.description, Some("Test credit grant".to_string()));
    }

    // Test: Standard user cannot create transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_create_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": other_user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "source_id": user.id.to_string(),
            "description": "Unauthorized attempt"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status_forbidden();
    }

    // Test: PlatformManager can create transactions (has same permissions as BillingManager)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_create_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "source_id": platform_manager.id.to_string(),
            "description": "Test credit grant from PlatformManager"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("100.0").unwrap());
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.source_id, platform_manager.id.to_string());
        assert_eq!(transaction.description, Some("Test credit grant from PlatformManager".to_string()));
    }

    // Test: RequestViewer user cannot create transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_cannot_create_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": other_user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "source_id": user.id.to_string(),
            "description": "Unauthorized attempt"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status_forbidden();
    }

    // Test: GET /transactions/{id} returns own transaction for standard user
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_transaction_as_standard_user(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "50.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.id, transaction_id);
        assert_eq!(transaction.amount, Decimal::from_str("50.0").unwrap());
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.description, Some("Initial credit grant".to_string()));
    }

    // Test: GET /transactions/{id} returns 404 for other user's transaction (not 403)
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_transaction_returns_404(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for user2
        let transaction_id = create_initial_credit_transaction(&pool, user2.id, "50.0").await;

        // user1 tries to access user2's transaction
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        // Should return 404 (not 403) to avoid leaking transaction existence
        response.assert_status_not_found();
    }

    // Test: BillingManager can view any user's transaction
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_view_any_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "75.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("75.0").unwrap());
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.description, Some("Initial credit grant".to_string()));
    }

    // Test: GET /transactions without query params returns only own transactions for standard user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_returns_own_for_standard_user(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // user1 lists transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see their own transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: GET /transactions?user_id=X returns 403 for standard user querying another user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_other_user_id_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // user1 tries to list user2's transactions
        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: BillingManager without params returns only own transactions (changed behavior)
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_list_all_transactions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for all users
        create_initial_credit_transaction(&pool, billing_manager.id, "50.0").await;
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // Without all=true, should only see own transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see billing_manager's own transactions
        assert!(transactions.iter().all(|t| t.user_id == billing_manager.id));
    }

    // Test: BillingManager can filter transactions by user_id
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_filter_by_user_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user1.id))
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see user1's transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: Create transaction validates amount > 0 (zero amount)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_amount_zero(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try zero amount
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "0",
            "source_id": billing_manager.id.to_string(),
            "description": "Invalid zero amount"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status_bad_request();
    }

    // Test: Create transaction validates amount > 0 (negative amount)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_amount_negative(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try negative amount
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "-50.0",
            "source_id": billing_manager.id.to_string(),
            "description": "Invalid negative amount"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status_bad_request();
    }

    // Test: Create transaction validates transaction type (rejects invalid types at deserialization)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_type(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try invalid transaction type (usage/purchase not allowed in API model)
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "usage",
            "amount": "10.0",
            "description": "Invalid type"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        // Returns 422 Unprocessable Entity because serde can't deserialize invalid enum value
        response.assert_status(axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Test: Create transaction validates user_id is provided, provides 422
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_requires_user_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;

        let transaction_data = json!({
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "source_id": billing_manager.id.to_string(),
            "description": "Missing user_id"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status_unprocessable_entity();
    }

    // Test: Create transaction checks for insufficient balance on removal
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_insufficient_balance(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Grant 50 credits
        create_initial_credit_transaction(&pool, user.id, "50.0").await;

        // Try to remove 100 credits
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_removal",
            "amount": "100.0",
            "source_id": billing_manager.id.to_string(),
            "description": "Over removal"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .json(&transaction_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("100.0").unwrap());
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminRemoval);
        assert_eq!(transaction.source_id, billing_manager.id.to_string());
        assert_eq!(transaction.description, Some("Over removal".to_string()));
        // Note: balance_after is no longer returned in API response - use page_start_balance
        // for balance context when listing transactions
    }

    // Test: GET /transactions/{id} returns own transaction for RequestViewer
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_transaction_as_request_viewer(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "50.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.id, transaction_id);
    }

    // Test: GET /transactions/{id} returns 404 for other user's transaction (RequestViewer)
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_transaction_returns_404_request_viewer(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for user2
        let transaction_id = create_initial_credit_transaction(&pool, user2.id, "50.0").await;

        // user1 tries to access user2's transaction
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        // Should return 404 (not 403) to avoid leaking transaction existence
        response.assert_status_not_found();
    }

    // Test: PlatformManager can view any user's transaction (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_view_any_transaction(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "75.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
    }

    // Test: GET /transactions without query params returns only own transactions for RequestViewer
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_returns_own_for_request_viewer(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // user1 lists transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see their own transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: PlatformManager without params returns only own transactions (changed behavior)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_list_all_transactions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for all users
        create_initial_credit_transaction(&pool, platform_manager.id, "50.0").await;
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // Without all=true, should only see own transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see platform_manager's own transactions
        assert!(transactions.iter().all(|t| t.user_id == platform_manager.id));
    }

    // Test: GET /transactions?user_id=X returns 403 for RequestViewer querying another user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_other_user_id_forbidden_request_viewer(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // user1 tries to list user2's transactions
        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: PlatformManager can filter transactions by user_id (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_filter_by_user_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user1.id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see user1's transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: Pagination works for GET /transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_pagination(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create 5 transactions
        for i in 1..=5 {
            create_initial_credit_transaction(&pool, user.id, &format!("{}.0", i * 10)).await;
        }

        // Test limit
        let response = app
            .get("/admin/api/v1/transactions?limit=2")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;
        assert_eq!(transactions.len(), 2);

        // Test skip
        let response = app
            .get("/admin/api/v1/transactions?skip=2&limit=2")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;
        assert_eq!(transactions.len(), 2);
    }

    // Test: BillingManager without params returns own transactions (not all)
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_without_params_returns_own(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for all users
        create_initial_credit_transaction(&pool, billing_manager.id, "50.0").await;
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should only see their own transactions
        assert!(transactions.iter().all(|t| t.user_id == billing_manager.id));
        assert!(!transactions.is_empty());
    }

    // Test: BillingManager with all=true returns all transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_with_all_returns_all_transactions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for all users
        create_initial_credit_transaction(&pool, billing_manager.id, "50.0").await;
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/transactions?all=true")
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should see transactions from all users
        assert!(transactions.iter().any(|t| t.user_id == billing_manager.id));
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    // Test: Standard user with all=true returns 403
    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_with_all_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/transactions?all=true")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: all=true takes precedence over user_id parameter
    #[sqlx::test]
    #[test_log::test]
    async fn test_all_takes_precedence_over_user_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // Request with both all=true and user_id - all should take precedence
        let response = app
            .get(&format!("/admin/api/v1/transactions?all=true&user_id={}", user1.id))
            .add_header(&add_auth_headers(&billing_manager)[0].0, &add_auth_headers(&billing_manager)[0].1)
            .add_header(&add_auth_headers(&billing_manager)[1].0, &add_auth_headers(&billing_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should see all transactions, not just user1's
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    // Test: PlatformManager with all=true returns all transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_with_all_returns_all_transactions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for all users
        create_initial_credit_transaction(&pool, platform_manager.id, "50.0").await;
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/transactions?all=true")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should see transactions from all users
        assert!(transactions.iter().any(|t| t.user_id == platform_manager.id));
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    // Test: RequestViewer with all=true returns 403
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_with_all_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;

        let response = app
            .get("/admin/api/v1/transactions?all=true")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    // Regression test: Ensure high-precision decimals can be serialized to JSON without panic
    // Previously, DECIMAL(64,32) values caused "CapacityError: insufficient capacity" panic
    // during rust_decimal string conversion in JSON serialization
    #[sqlx::test]
    #[test_log::test]
    async fn test_high_precision_decimal_serialization(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create transaction with maximum precision (15 decimal places)
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "123.456789012345678").await;

        // Test GET single transaction - this triggers JSON serialization
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Should not panic and return 200
        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();

        // Verify the high-precision value was serialized correctly
        assert_eq!(transaction.id, transaction_id);
        assert_eq!(transaction.user_id, user.id);
        // Note: JSON serialization converts Decimal to f64, so exact comparison may not match
        // but the important part is that it didn't panic

        // Test GET list transactions - this also triggers JSON serialization of Vec<CreditTransactionResponse>
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should have at least our transaction
        assert!(!transactions.is_empty());
        assert!(transactions.iter().any(|t| t.id == transaction_id));
    }

    // Test: Verify that Decimal serialization preserves arbitrary precision
    #[sqlx::test]
    #[test_log::test]
    async fn test_decimal_precision_preserved_in_json(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create transaction with high precision that would be lost if serialized as f64
        // f64 has ~15-17 decimal digits of precision total
        let precise_amount = "0.123456789012345"; // 15 decimal places
        let transaction_id = create_initial_credit_transaction(&pool, user.id, precise_amount).await;

        // Get the transaction via API
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();

        // Get the raw JSON text to inspect how Decimal is serialized
        let json_text = response.text();

        // Check if amount is serialized as a string (preserves precision) or number (loses precision)
        // rust_decimal's default Serialize implementation uses strings for arbitrary precision
        let json_value: serde_json::Value = serde_json::from_str(&json_text).expect("Failed to parse JSON");

        // Print for debugging
        println!("JSON amount field: {:?}", json_value["amount"]);

        // Check what type the amount field is
        match &json_value["amount"] {
            serde_json::Value::String(s) => {
                println!("✓ Amount serialized as string (arbitrary precision): {}", s);
                assert_eq!(s, precise_amount, "String representation should match exactly");
            }
            serde_json::Value::Number(n) => {
                println!("✗ Amount serialized as number (may lose precision): {}", n);
                // If it's a number, precision might be lost
                // f64 representation would be something like 0.12345678901234501
            }
            other => {
                panic!("Unexpected JSON type for amount: {:?}", other);
            }
        }

        // Test round-trip: deserialize and verify precision is preserved
        let transaction: CreditTransactionResponse = serde_json::from_str(&json_text).expect("Failed to deserialize");
        assert_eq!(transaction.amount.to_string(), precise_amount);
        // Note: balance_after is no longer in the API response - use page_start_balance
        // for balance context when listing transactions
    }

    // Test: Batch grouping aggregates correctly with mixed transaction types
    #[sqlx::test]
    #[test_log::test]
    async fn test_batch_grouping_with_mixed_transactions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create diverse transaction data
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = CreditsHandler::new(&mut conn);

        // 1. Admin grant
        let grant_request = CreditTransactionCreateDBRequest::admin_grant(
            user.id,
            user.id,
            Decimal::from_str("1000.0").unwrap(),
            Some("Initial grant".to_string()),
        );
        credits_repo
            .create_transaction(&grant_request)
            .await
            .expect("Failed to create grant");

        // 2. Purchase
        let purchase_request = CreditTransactionCreateDBRequest {
            user_id: user.id,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str("500.0").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: Some("Purchase".to_string()),
            fusillade_batch_id: None,
        };
        credits_repo
            .create_transaction(&purchase_request)
            .await
            .expect("Failed to create purchase");

        // 3. Create batch data in http_analytics and usage transactions
        let batch_id_1 = Uuid::new_v4();
        let batch_id_2 = Uuid::new_v4();

        // Batch 1: 5 requests with gpt-4
        for i in 0..5 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW(), $3, $4, $5, $6, $7)
                RETURNING id
                "#,
                Uuid::new_v4(),            // instance_id
                i as i64,                  // correlation_id
                "POST",                    // method
                "/ai/v1/chat/completions", // uri
                "gpt-4",                   // model
                user.id,                   // user_id
                batch_id_1                 // fusillade_batch_id
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str(&format!("{}.0", i + 1)).unwrap(), // 1.0, 2.0, 3.0, 4.0, 5.0
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Batch 1 request {}", i)),
                fusillade_batch_id: Some(batch_id_1),
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        // Batch 2: 3 requests with gpt-3.5-turbo
        for i in 0..3 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW(), $3, $4, $5, $6, $7)
                RETURNING id
                "#,
                Uuid::new_v4(),
                (5 + i) as i64, // correlation_id (continue from batch 1)
                "POST",
                "/ai/v1/chat/completions",
                "gpt-3.5-turbo",
                user.id,
                batch_id_2
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str(&format!("{}.0", (i + 1) * 10)).unwrap(), // 10.0, 20.0, 30.0
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Batch 2 request {}", i)),
                fusillade_batch_id: Some(batch_id_2),
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        // 4. Individual usage transactions (not in a batch)
        for i in 0..2 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW(), $3, $4, $5, $6, NULL)
                RETURNING id
                "#,
                Uuid::new_v4(),
                (8 + i) as i64, // correlation_id
                "POST",
                "/ai/v1/chat/completions",
                "claude-3-sonnet",
                user.id
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str(&format!("{}.0", i + 1)).unwrap(), // 1.0, 2.0
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Individual request {}", i)),
                fusillade_batch_id: None, // Not in a batch
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        drop(conn);

        // Test 1: WITHOUT batch grouping - should see all 12 individual transactions
        let response = app
            .get("/admin/api/v1/transactions?group_batches=false&limit=50")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should have: 1 grant + 1 purchase + 5 batch1 + 3 batch2 + 2 individual = 12 total
        assert_eq!(transactions.len(), 12, "Should have 12 individual transactions without grouping");

        // Verify we have the expected transaction types
        let grant_count = transactions
            .iter()
            .filter(|t| t.transaction_type == CreditTransactionType::AdminGrant)
            .count();
        let purchase_count = transactions
            .iter()
            .filter(|t| t.transaction_type == CreditTransactionType::Purchase)
            .count();
        let usage_count = transactions
            .iter()
            .filter(|t| t.transaction_type == CreditTransactionType::Usage)
            .count();

        assert_eq!(grant_count, 1, "Should have 1 admin grant");
        assert_eq!(purchase_count, 1, "Should have 1 purchase");
        assert_eq!(usage_count, 10, "Should have 10 usage transactions (5 + 3 + 2)");

        // Test 2: WITH batch grouping - should see aggregated batches
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=50")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let response_body: TransactionListResponse = response.json();
        let transactions = &response_body.data;

        // Should have: 1 grant + 1 purchase + 1 batch1 + 1 batch2 + 2 individual = 6 total
        assert_eq!(transactions.len(), 6, "Should have 6 transactions with batch grouping");

        // Find the batched transactions by their aggregated amounts
        // Batch 1: 1.0 + 2.0 + 3.0 + 4.0 + 5.0 = 15.0
        // Batch 2: 10.0 + 20.0 + 30.0 = 60.0
        let batch_1_txn = transactions
            .iter()
            .find(|t| t.description == Some("Batch".to_string()) && t.amount == Decimal::from_str("15.0").unwrap())
            .expect("Should have batch 1 aggregated transaction (amount 15.0)");

        let batch_2_txn = transactions
            .iter()
            .find(|t| t.description == Some("Batch".to_string()) && t.amount == Decimal::from_str("60.0").unwrap())
            .expect("Should have batch 2 aggregated transaction (amount 60.0)");

        // Verify batch 1 has batch_id
        assert!(batch_1_txn.batch_id.is_some(), "Batch 1 should have batch_id");

        // Verify batch 2 has batch_id
        assert!(batch_2_txn.batch_id.is_some(), "Batch 2 should have batch_id");

        // Verify individual usage transactions are still present
        let individual_usage_count = transactions
            .iter()
            .filter(|t| t.transaction_type == CreditTransactionType::Usage && t.batch_id.is_none())
            .count();
        assert_eq!(individual_usage_count, 2, "Should still have 2 individual usage transactions");
    }

    // Test: Batch grouping pagination works correctly
    #[sqlx::test]
    #[test_log::test]
    async fn test_batch_grouping_pagination(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = CreditsHandler::new(&mut conn);

        // Create initial balance
        let grant_request = CreditTransactionCreateDBRequest::admin_grant(
            user.id,
            user.id,
            Decimal::from_str("10000.0").unwrap(),
            Some("Initial grant".to_string()),
        );
        credits_repo
            .create_transaction(&grant_request)
            .await
            .expect("Failed to create grant");

        // Create 5 batches with 10 requests each
        for batch_num in 0..5 {
            let batch_id = Uuid::new_v4();
            for req_num in 0..10 {
                let analytics_record = sqlx::query!(
                    r#"
                    INSERT INTO http_analytics
                        (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                    VALUES ($1, $2, NOW(), $3, $4, $5, $6, $7)
                    RETURNING id
                    "#,
                    Uuid::new_v4(),
                    (batch_num * 10 + req_num) as i64,
                    "POST",
                    "/ai/v1/chat/completions",
                    format!("model-{}", batch_num),
                    user.id,
                    batch_id
                )
                .fetch_one(&pool)
                .await
                .expect("Failed to insert analytics");

                let usage_request = CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Usage,
                    amount: Decimal::from_str("1.0").unwrap(),
                    source_id: analytics_record.id.to_string(),
                    description: Some(format!("Batch {} request {}", batch_num, req_num)),
                    fusillade_batch_id: Some(batch_id),
                };
                credits_repo
                    .create_transaction(&usage_request)
                    .await
                    .expect("Failed to create usage");
            }
        }

        drop(conn);

        // Test pagination with grouping
        // Without grouping: 1 grant + 50 usage = 51 transactions
        // With grouping: 1 grant + 5 batches = 6 transactions

        // Page 1: limit=3, skip=0
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=3&skip=0")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page1_body: TransactionListResponse = response.json();
        let page1 = &page1_body.data;
        assert_eq!(page1.len(), 3, "Page 1 should have 3 transactions");

        // Page 2: limit=3, skip=3
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=3&skip=3")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page2_body: TransactionListResponse = response.json();
        let page2 = &page2_body.data;
        assert_eq!(page2.len(), 3, "Page 2 should have 3 transactions");

        // Page 3: limit=3, skip=6 (should have 0 since we only have 6 total)
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=3&skip=6")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page3_body: TransactionListResponse = response.json();
        let page3 = &page3_body.data;
        assert_eq!(page3.len(), 0, "Page 3 should be empty");

        // Verify no duplicates across pages
        let mut all_ids = vec![];
        all_ids.extend(page1.iter().map(|t| t.id));
        all_ids.extend(page2.iter().map(|t| t.id));

        let unique_ids: std::collections::HashSet<_> = all_ids.iter().collect();
        assert_eq!(all_ids.len(), unique_ids.len(), "Should have no duplicate IDs across pages");
    }

    // Test: page_start_balance is calculated correctly with batch grouping
    // This is a regression test for the bug where page_start_balance was calculated using
    // raw transaction counts instead of grouped transaction counts
    #[sqlx::test]
    #[test_log::test]
    async fn test_page_start_balance_with_batch_grouping(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = CreditsHandler::new(&mut conn);

        // Create initial balance: $1000
        let grant_request = CreditTransactionCreateDBRequest::admin_grant(
            user.id,
            user.id,
            Decimal::from_str("1000.0").unwrap(),
            Some("Initial grant".to_string()),
        );
        credits_repo
            .create_transaction(&grant_request)
            .await
            .expect("Failed to create grant");

        // Create 3 batches, each with multiple transactions
        // Batch 1: 5 transactions of $2 each = $10 total (oldest)
        let batch_id_1 = Uuid::new_v4();
        for i in 0..5 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW() - interval '3 hours', $3, $4, $5, $6, $7)
                RETURNING id
                "#,
                Uuid::new_v4(),
                i as i64,
                "POST",
                "/ai/v1/chat/completions",
                "gpt-4",
                user.id,
                batch_id_1
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str("2.0").unwrap(),
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Batch 1 request {}", i)),
                fusillade_batch_id: Some(batch_id_1),
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        // Batch 2: 10 transactions of $3 each = $30 total (middle)
        let batch_id_2 = Uuid::new_v4();
        for i in 0..10 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW() - interval '2 hours', $3, $4, $5, $6, $7)
                RETURNING id
                "#,
                Uuid::new_v4(),
                (5 + i) as i64,
                "POST",
                "/ai/v1/chat/completions",
                "gpt-4",
                user.id,
                batch_id_2
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str("3.0").unwrap(),
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Batch 2 request {}", i)),
                fusillade_batch_id: Some(batch_id_2),
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        // Batch 3: 3 transactions of $5 each = $15 total (newest)
        let batch_id_3 = Uuid::new_v4();
        for i in 0..3 {
            let analytics_record = sqlx::query!(
                r#"
                INSERT INTO http_analytics
                    (instance_id, correlation_id, timestamp, method, uri, model, user_id, fusillade_batch_id)
                VALUES ($1, $2, NOW() - interval '1 hour', $3, $4, $5, $6, $7)
                RETURNING id
                "#,
                Uuid::new_v4(),
                (15 + i) as i64,
                "POST",
                "/ai/v1/chat/completions",
                "gpt-4",
                user.id,
                batch_id_3
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to insert analytics");

            let usage_request = CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str("5.0").unwrap(),
                source_id: analytics_record.id.to_string(),
                description: Some(format!("Batch 3 request {}", i)),
                fusillade_batch_id: Some(batch_id_3),
            };
            credits_repo
                .create_transaction(&usage_request)
                .await
                .expect("Failed to create usage");
        }

        // Add a non-batched payment (newest transaction)
        let payment_request = CreditTransactionCreateDBRequest {
            user_id: user.id,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str("100.0").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: Some("Dummy payment (test)".to_string()),
            fusillade_batch_id: None,
        };
        credits_repo
            .create_transaction(&payment_request)
            .await
            .expect("Failed to create payment");

        drop(conn);

        // Expected state:
        // - Initial grant: +$1000 (balance after: $1000)
        // - Batch 1: -$10 (balance after: $990)
        // - Batch 2: -$30 (balance after: $960)
        // - Batch 3: -$15 (balance after: $945)
        // - Payment: +$100 (balance after: $1045)
        //
        // Grouped order (newest first by max_seq):
        // 1. Payment: +$100 (balance after: $1045)
        // 2. Batch 3: -$15 (balance after: $945)
        // 3. Batch 2: -$30 (balance after: $960)
        // 4. Batch 1: -$10 (balance after: $990)
        // 5. Initial grant: +$1000 (balance after: $1000)
        //
        // Total: 5 grouped items (1 grant + 3 batches + 1 payment)

        // Test page 1 (skip=0, limit=2): Should show Payment and Batch 3
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=2&skip=0")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page1: TransactionListResponse = response.json();

        // page_start_balance for skip=0 should be current balance = $1045
        assert_eq!(
            page1.page_start_balance,
            Decimal::from_str("1045.0").unwrap(),
            "Page 1 page_start_balance should be current balance $1045"
        );
        assert_eq!(page1.data.len(), 2, "Page 1 should have 2 items");

        // Verify the transactions on page 1 (newest first)
        // First item should be the payment (+$100)
        assert_eq!(
            page1.data[0].description,
            Some("Dummy payment (test)".to_string()),
            "First item should be the payment"
        );
        assert_eq!(
            page1.data[0].amount,
            Decimal::from_str("100.0").unwrap(),
            "Payment amount should be $100"
        );

        // Second item should be Batch 3 ($15 total)
        assert_eq!(
            page1.data[1].description,
            Some("Batch".to_string()),
            "Second item should be Batch 3"
        );
        assert_eq!(
            page1.data[1].amount,
            Decimal::from_str("15.0").unwrap(),
            "Batch 3 amount should be $15"
        );

        // Test page 2 (skip=2, limit=2): Should show Batch 2 and Batch 1
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=2&skip=2")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page2: TransactionListResponse = response.json();

        // page_start_balance for skip=2 should be balance after skipping Payment (+$100) and Batch 3 (-$15)
        // Current balance ($1045) - Payment (+$100) - Batch 3 (-$15) = $1045 - $100 + $15 = $960
        // Wait, that's wrong. Let me recalculate:
        // page_start_balance = current_balance - sum_of_skipped_items
        // sum_of_skipped = +$100 (payment) + (-$15) (batch 3) = $85
        // page_start_balance = $1045 - $85 = $960
        assert_eq!(
            page2.page_start_balance,
            Decimal::from_str("960.0").unwrap(),
            "Page 2 page_start_balance should be $960 (balance after Payment and Batch 3)"
        );
        assert_eq!(page2.data.len(), 2, "Page 2 should have 2 items");

        // First item on page 2 should be Batch 2 ($30 total)
        assert_eq!(
            page2.data[0].description,
            Some("Batch".to_string()),
            "First item on page 2 should be Batch 2"
        );
        assert_eq!(
            page2.data[0].amount,
            Decimal::from_str("30.0").unwrap(),
            "Batch 2 amount should be $30"
        );

        // Second item on page 2 should be Batch 1 ($10 total)
        assert_eq!(
            page2.data[1].description,
            Some("Batch".to_string()),
            "Second item on page 2 should be Batch 1"
        );
        assert_eq!(
            page2.data[1].amount,
            Decimal::from_str("10.0").unwrap(),
            "Batch 1 amount should be $10"
        );

        // Test page 3 (skip=4, limit=2): Should show only the Initial grant
        let response = app
            .get("/admin/api/v1/transactions?group_batches=true&limit=2&skip=4")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let page3: TransactionListResponse = response.json();

        // page_start_balance for skip=4 should be balance after skipping all except the grant
        // sum_of_skipped = +$100 + (-$15) + (-$30) + (-$10) = $100 - $55 = $45
        // page_start_balance = $1045 - $45 = $1000
        assert_eq!(
            page3.page_start_balance,
            Decimal::from_str("1000.0").unwrap(),
            "Page 3 page_start_balance should be $1000 (balance after initial grant only)"
        );
        assert_eq!(page3.data.len(), 1, "Page 3 should have 1 item (the initial grant)");

        // The only item should be the initial grant
        assert_eq!(
            page3.data[0].description,
            Some("Initial grant".to_string()),
            "Only item on page 3 should be the initial grant"
        );
        assert_eq!(
            page3.data[0].amount,
            Decimal::from_str("1000.0").unwrap(),
            "Initial grant amount should be $1000"
        );

        // Verify total count is correct (5 grouped items)
        assert_eq!(page1.total_count, 5, "Total count should be 5 grouped items");
    }

    // Test: page_start_balance is calculated correctly when filtering by date range
    // This is a regression test for the bug where page_start_balance showed current balance
    // instead of the balance at the end of the filtered date range
    #[sqlx::test]
    #[test_log::test]
    async fn test_page_start_balance_with_date_filter(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create all transactions with specific timestamps using raw SQL
        // Initial grant: +$1000 at 5 hours ago (oldest)
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, created_at)
            VALUES ($1, 'admin_grant', $2, $3, $4, NOW() - interval '5 hours')
            "#,
            user.id,
            Decimal::from_str("1000.0").unwrap(),
            Uuid::new_v4().to_string(),
            "Initial grant",
        )
        .execute(&pool)
        .await
        .expect("Failed to create grant");

        // Transaction 1: -$100 at 3 hours ago
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, created_at)
            VALUES ($1, 'usage', $2, $3, $4, NOW() - interval '3 hours')
            "#,
            user.id,
            Decimal::from_str("100.0").unwrap(),
            Uuid::new_v4().to_string(),
            "Usage 3 hours ago",
        )
        .execute(&pool)
        .await
        .expect("Failed to create transaction");

        // Transaction 2: -$50 at 2 hours ago
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, created_at)
            VALUES ($1, 'usage', $2, $3, $4, NOW() - interval '2 hours')
            "#,
            user.id,
            Decimal::from_str("50.0").unwrap(),
            Uuid::new_v4().to_string(),
            "Usage 2 hours ago",
        )
        .execute(&pool)
        .await
        .expect("Failed to create transaction");

        // Transaction 3: -$25 at 1 hour ago
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, created_at)
            VALUES ($1, 'usage', $2, $3, $4, NOW() - interval '1 hour')
            "#,
            user.id,
            Decimal::from_str("25.0").unwrap(),
            Uuid::new_v4().to_string(),
            "Usage 1 hour ago",
        )
        .execute(&pool)
        .await
        .expect("Failed to create transaction");

        // Transaction 4: +$200 at 30 minutes ago (most recent)
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, created_at)
            VALUES ($1, 'purchase', $2, $3, $4, NOW() - interval '30 minutes')
            "#,
            user.id,
            Decimal::from_str("200.0").unwrap(),
            Uuid::new_v4().to_string(),
            "Purchase 30 minutes ago",
        )
        .execute(&pool)
        .await
        .expect("Failed to create transaction");

        // Expected balances:
        // Initial: $1000
        // After -$100: $900
        // After -$50: $850
        // After -$25: $825
        // After +$200: $1025 (current balance)

        // Test 1: No date filter - page_start_balance should be current balance ($1025)
        let response = app
            .get("/admin/api/v1/transactions?limit=10")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let no_filter: TransactionListResponse = response.json();
        assert_eq!(
            no_filter.page_start_balance,
            Decimal::from_str("1025.0").unwrap(),
            "Without date filter, page_start_balance should be current balance $1025"
        );

        // Test 2: Filter with end_date = 90 minutes ago (excludes the +$200 purchase and -$25 usage)
        // Should show transactions from 3 hours ago and 2 hours ago only
        // Balance at that point: $1000 - $100 - $50 = $850
        let end_date = (chrono::Utc::now() - chrono::Duration::minutes(90))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let start_date = (chrono::Utc::now() - chrono::Duration::hours(4))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        let response = app
            .get(&format!(
                "/admin/api/v1/transactions?limit=10&start_date={}&end_date={}",
                start_date, end_date
            ))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let date_filtered: TransactionListResponse = response.json();

        // The page_start_balance should be the balance at the end_date point
        // Current balance ($1025) - transactions after end_date (+$200 - $25 = +$175) = $850
        assert_eq!(
            date_filtered.page_start_balance,
            Decimal::from_str("850.0").unwrap(),
            "With end_date filter, page_start_balance should be $850 (balance at that time)"
        );

        // Should only show 2 transactions (the ones within the date range)
        assert_eq!(date_filtered.data.len(), 2, "Should have 2 transactions in the filtered range");

        // Test 3: Filter with end_date = 2.5 hours ago (excludes everything except the oldest usage)
        // Balance at that point: $1000 - $100 = $900
        let end_date_earlier = (chrono::Utc::now() - chrono::Duration::minutes(150))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        let response = app
            .get(&format!(
                "/admin/api/v1/transactions?limit=10&start_date={}&end_date={}",
                start_date, end_date_earlier
            ))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let earlier_filtered: TransactionListResponse = response.json();

        // Current balance ($1025) - transactions after end_date (+$200 - $25 - $50 = +$125) = $900
        assert_eq!(
            earlier_filtered.page_start_balance,
            Decimal::from_str("900.0").unwrap(),
            "With earlier end_date filter, page_start_balance should be $900"
        );

        assert_eq!(
            earlier_filtered.data.len(),
            1,
            "Should have 1 transaction in the earlier filtered range"
        );
    }
}
