use crate::{
    api::models::{
        transactions::{CreditTransactionCreate, CreditTransactionResponse, ListTransactionsQuery},
        users::CurrentUser,
    },
    auth::permissions::{self, operation, resource, RequiresPermission},
    db::{
        handlers::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    errors::{Error, Result},
    types::{Operation, Permission, Resource},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use rust_decimal::Decimal;
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
        ("X-Doubleword-User" = [])
    )
)]
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
        description: data.description,
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
        ("X-Doubleword-User" = [])
    )
)]
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
    description = "Get a list of credit transactions. Non-BillingManager users can only see their own transactions. Use user_id parameter to filter by user (BillingManager only).",
    params(
        ListTransactionsQuery
    ),
    responses(
        (status = 200, description = "List of transactions", body = [CreditTransactionResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - cannot access other users' transactions"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_transactions(
    State(state): State<AppState>,
    Query(query): Query<ListTransactionsQuery>,
    current_user: CurrentUser,
) -> Result<Json<Vec<CreditTransactionResponse>>> {
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    // Check if user has ReadAll permission
    let has_read_all = permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll);

    // Determine which user_id to filter by
    let filter_user_id = match query.user_id {
        Some(requested_user_id) => {
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
        None => {
            // If no user_id specified
            if has_read_all {
                None // BillingManager can see all
            } else {
                Some(current_user.id) // Others see only their own
            }
        }
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let transactions = if let Some(user_id) = filter_user_id {
        repo.list_user_transactions(user_id, skip, limit).await?
    } else {
        repo.list_all_transactions(skip, limit).await?
    };

    Ok(Json(transactions.into_iter().map(CreditTransactionResponse::from).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::Role,
        db::{handlers::Credits as CreditsHandler, models::credits::CreditTransactionCreateDBRequest},
        test_utils::*,
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
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: amount_decimal,
            description: Some("Initial credit grant".to_string()),
        };

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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Test credit grant"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("100.0").unwrap());
    }

    // Test: Standard user cannot create transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_create_transaction(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": other_user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Unauthorized attempt"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&transaction_data)
            .await;

        response.assert_status_forbidden();
    }

    // Test: PlatformManager can create transactions (has same permissions as BillingManager)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_create_transaction(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Test credit grant from PlatformManager"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.amount, Decimal::from_str("100.0").unwrap());
    }

    // Test: RequestViewer user cannot create transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_cannot_create_transaction(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let transaction_data = json!({
            "user_id": other_user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Unauthorized attempt"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&transaction_data)
            .await;

        response.assert_status_forbidden();
    }

    // Test: GET /transactions/{id} returns own transaction for standard user
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_transaction_as_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "50.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.id, transaction_id);
    }

    // Test: GET /transactions/{id} returns 404 for other user's transaction (not 403)
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_transaction_returns_404(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for user2
        let transaction_id = create_initial_credit_transaction(&pool, user2.id, "50.0").await;

        // user1 tries to access user2's transaction
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        // Should return 404 (not 403) to avoid leaking transaction existence
        response.assert_status_not_found();
    }

    // Test: BillingManager can view any user's transaction
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_view_any_transaction(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "75.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
    }

    // Test: GET /transactions without query params returns only own transactions for standard user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_returns_own_for_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // user1 lists transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should only see their own transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: GET /transactions?user_id=X returns 403 for standard user querying another user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_other_user_id_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // user1 tries to list user2's transactions
        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: BillingManager can list all transactions without filter
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_list_all_transactions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should see transactions from both users
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    // Test: BillingManager can filter transactions by user_id
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_filter_by_user_id(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user1.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should only see user1's transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: Create transaction validates amount > 0 (zero amount)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_amount_zero(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try zero amount
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "0",
            "description": "Invalid zero amount"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status_bad_request();
    }

    // Test: Create transaction validates amount > 0 (negative amount)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_amount_negative(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try negative amount
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_grant",
            "amount": "-50.0",
            "description": "Invalid negative amount"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status_bad_request();
    }

    // Test: Create transaction validates transaction type (rejects invalid types at deserialization)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_validates_type(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
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
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        // Returns 422 Unprocessable Entity because serde can't deserialize invalid enum value
        response.assert_status(axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Test: Create transaction validates user_id is provided, provides 422
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_requires_user_id(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;

        let transaction_data = json!({
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Missing user_id"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status_unprocessable_entity();
    }

    // Test: Create transaction checks for insufficient balance on removal
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_insufficient_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Grant 50 credits
        create_initial_credit_transaction(&pool, user.id, "50.0").await;

        // Try to remove 100 credits
        let transaction_data = json!({
            "user_id": user.id.to_string(),
            "transaction_type": "admin_removal",
            "amount": "100.0",
            "description": "Over removal"
        });

        let response = app
            .post("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction_data)
            .await;

        response.assert_status_bad_request();
    }

    // Test: GET /transactions/{id} returns own transaction for RequestViewer
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_transaction_as_request_viewer(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "50.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for user2
        let transaction_id = create_initial_credit_transaction(&pool, user2.id, "50.0").await;

        // user1 tries to access user2's transaction
        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        // Should return 404 (not 403) to avoid leaking transaction existence
        response.assert_status_not_found();
    }

    // Test: PlatformManager can view any user's transaction (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_view_any_transaction(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a transaction for the user
        let transaction_id = create_initial_credit_transaction(&pool, user.id, "75.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions/{}", transaction_id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let transaction: CreditTransactionResponse = response.json();
        assert_eq!(transaction.user_id, user.id);
    }

    // Test: GET /transactions without query params returns only own transactions for RequestViewer
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_returns_own_for_request_viewer(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        // user1 lists transactions
        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should only see their own transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: PlatformManager can list all transactions (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_list_all_transactions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/transactions")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should see transactions from both users
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    // Test: GET /transactions?user_id=X returns 403 for RequestViewer querying another user
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_other_user_id_forbidden_request_viewer(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::RequestViewer).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // user1 tries to list user2's transactions
        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: PlatformManager can filter transactions by user_id (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_filter_by_user_id(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create transactions for both users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get(&format!("/admin/api/v1/transactions?user_id={}", user1.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should only see user1's transactions
        assert!(transactions.iter().all(|t| t.user_id == user1.id));
    }

    // Test: Pagination works for GET /transactions
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_pagination(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create 5 transactions
        for i in 1..=5 {
            create_initial_credit_transaction(&pool, user.id, &format!("{}.0", i * 10)).await;
        }

        // Test limit
        let response = app
            .get("/admin/api/v1/transactions?limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 2);

        // Test skip
        let response = app
            .get("/admin/api/v1/transactions?skip=2&limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 2);
    }
}
