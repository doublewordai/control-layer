use crate::{
    api::models::{
        credits::{CreditTransactionCreate, CreditTransactionResponse, ListTransactionsQuery, UserBalanceResponse},
        users::CurrentUser,
    },
    auth::permissions::{operation, resource, RequiresPermission},
    db::{
        handlers::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    errors::{Error, Result},
    types::UserId,
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use rust_decimal::Decimal;

/// Get current user's credit balance
#[utoipa::path(
    get,
    path = "/users/current/credits/balance",
    tag = "credits",
    summary = "Get current user's credit balance",
    description = "Get the credit balance for the currently authenticated user",
    responses(
        (status = 200, description = "User's current balance", body = UserBalanceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_current_user_balance(
    State(state): State<AppState>,
    _perm: RequiresPermission<resource::Credits, operation::ReadOwn>,
    current_user: CurrentUser,
) -> Result<Json<UserBalanceResponse>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let balance = repo.get_user_balance(current_user.id).await?;

    Ok(Json(UserBalanceResponse {
        user_id: current_user.id,
        current_balance: balance,
    }))
}

/// List current user's credit transactions
#[utoipa::path(
    get,
    path = "/users/current/credits/transactions",
    tag = "credits",
    summary = "List current user's credit transactions",
    description = "Get transaction history for the currently authenticated user",
    params(
        ListTransactionsQuery
    ),
    responses(
        (status = 200, description = "List of transactions", body = [CreditTransactionResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_current_user_transactions(
    State(state): State<AppState>,
    Query(query): Query<ListTransactionsQuery>,
    _perm: RequiresPermission<resource::Credits, operation::ReadOwn>,
    current_user: CurrentUser,
) -> Result<Json<Vec<CreditTransactionResponse>>> {
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let transactions = repo.list_user_transactions(current_user.id, skip, limit).await?;

    Ok(Json(transactions.into_iter().map(CreditTransactionResponse::from).collect()))
}

/// Add credits to a user's account (BillingManager only)
#[utoipa::path(
    post,
    path = "/users/{user_id}/credits",
    tag = "credits",
    summary = "Add credits to user account",
    description = "Add or remove credits from a user's account (BillingManager role required)",
    params(
        ("user_id" = String, Path, description = "User ID (UUID)"),
    ),
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
pub async fn add_user_credits(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    _perm: RequiresPermission<resource::Credits, operation::CreateAll>,
    Json(data): Json<CreditTransactionCreate>,
) -> Result<(StatusCode, Json<CreditTransactionResponse>)> {
    // Validate that only admin_grant or admin_removal are allowed
    if !matches!(data.transaction_type, CreditTransactionType::AdminGrant | CreditTransactionType::AdminRemoval) {
        return Err(Error::BadRequest {
            message: "Only 'admin_grant' and 'admin_removal' transaction types are allowed for this endpoint".to_string(),
        });
    }

    // Validate amount is positive
    if data.amount <= Decimal::ZERO {
        return Err(Error::BadRequest {
            message: "Amount must be greater than zero".to_string(),
        });
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    // Get current balance
    let current_balance = repo.get_user_balance(user_id).await?;

    // Calculate new balance based on transaction type
    let new_balance = match data.transaction_type {
        CreditTransactionType::AdminGrant => current_balance + data.amount,
        CreditTransactionType::AdminRemoval => {
            let result = current_balance - data.amount;
            if result < Decimal::ZERO {
                return Err(Error::BadRequest {
                    message: format!(
                        "Insufficient credits: current balance is {}, cannot remove {}",
                        current_balance, data.amount
                    ),
                });
            }
            result
        }
        _ => unreachable!(), // Already validated above
    };

    // Create the transaction
    let db_request = CreditTransactionCreateDBRequest {
        user_id,
        transaction_type: data.transaction_type,
        amount: data.amount,
        balance_after: new_balance,
        description: data.description,
    };

    let transaction = repo.create_transaction(&db_request).await?;

    Ok((StatusCode::CREATED, Json(CreditTransactionResponse::from(transaction))))
}

/// Get a specific user's credit balance (BillingManager only)
#[utoipa::path(
    get,
    path = "/users/{user_id}/credits/balance",
    tag = "credits",
    summary = "Get user's credit balance",
    description = "Get the credit balance for a specific user (BillingManager role required)",
    params(
        ("user_id" = String, Path, description = "User ID (UUID)"),
    ),
    responses(
        (status = 200, description = "User's current balance", body = UserBalanceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - requires BillingManager role"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_user_balance(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    _perm: RequiresPermission<resource::Credits, operation::ReadAll>,
) -> Result<Json<UserBalanceResponse>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let balance = repo.get_user_balance(user_id).await?;

    Ok(Json(UserBalanceResponse {
        user_id,
        current_balance: balance,
    }))
}

/// List a specific user's credit transactions (BillingManager only)
#[utoipa::path(
    get,
    path = "/users/{user_id}/credits/transactions",
    tag = "credits",
    summary = "List user's credit transactions",
    description = "Get transaction history for a specific user (BillingManager role required)",
    params(
        ("user_id" = String, Path, description = "User ID (UUID)"),
        ListTransactionsQuery
    ),
    responses(
        (status = 200, description = "List of transactions", body = [CreditTransactionResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - requires BillingManager role"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_user_transactions(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    Query(query): Query<ListTransactionsQuery>,
    _perm: RequiresPermission<resource::Credits, operation::ReadAll>,
) -> Result<Json<Vec<CreditTransactionResponse>>> {
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let transactions = repo.list_user_transactions(user_id, skip, limit).await?;

    Ok(Json(transactions.into_iter().map(CreditTransactionResponse::from).collect()))
}

/// Get all users' credit balances (BillingManager only)
#[utoipa::path(
    get,
    path = "/credits/balances",
    tag = "credits",
    summary = "List all users' credit balances",
    description = "Get credit balances for all users (BillingManager role required)",
    responses(
        (status = 200, description = "List of all user balances", body = [UserBalanceResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - requires BillingManager role"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_all_user_balances(
    State(state): State<AppState>,
    _perm: RequiresPermission<resource::Credits, operation::ReadAll>,
) -> Result<Json<Vec<UserBalanceResponse>>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let balances = repo.list_all_user_balances().await?;

    Ok(Json(balances.into_iter().map(UserBalanceResponse::from).collect()))
}

/// List all credit transactions across all users (BillingManager only)
#[utoipa::path(
    get,
    path = "/credits/transactions",
    tag = "credits",
    summary = "List all credit transactions",
    description = "Get all credit transactions across all users (BillingManager role required)",
    params(
        ListTransactionsQuery
    ),
    responses(
        (status = 200, description = "List of all transactions", body = [CreditTransactionResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - requires BillingManager role"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_all_transactions(
    State(state): State<AppState>,
    Query(query): Query<ListTransactionsQuery>,
    _perm: RequiresPermission<resource::Credits, operation::ReadAll>,
) -> Result<Json<Vec<CreditTransactionResponse>>> {
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Credits::new(&mut pool_conn);

    let transactions = repo.list_all_transactions(skip, limit).await?;

    Ok(Json(transactions.into_iter().map(CreditTransactionResponse::from).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::db::handlers::Credits as CreditsHandler;
    use crate::db::models::credits::CreditTransactionCreateDBRequest;
    use crate::test_utils::{add_auth_headers, create_test_admin_user, create_test_app, create_test_user};
    use rust_decimal::Decimal;
    use serde_json::json;
    use sqlx::PgPool;
    use std::str::FromStr;

    async fn create_test_billing_manager_user(pool: &PgPool) -> crate::api::models::users::UserResponse {
        create_test_user(pool, Role::BillingManager).await
    }

    async fn create_initial_credit_transaction(pool: &PgPool, user_id: UserId, amount: &str) -> i64 {
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = CreditsHandler::new(&mut conn);

        // Get current balance
        let current_balance = credits_repo.get_user_balance(user_id).await.expect("Failed to get balance");

        let amount_decimal = Decimal::from_str(amount).expect("Invalid decimal amount");
        let new_balance = current_balance + amount_decimal;

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: amount_decimal,
            balance_after: new_balance,
            description: Some("Initial credit grant".to_string()),
        };

        credits_repo.create_transaction(&request).await.expect("Failed to create transaction").id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Initially should return 0 balance
        let response = app
            .get("/admin/api/v1/users/current/credits/balance")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserBalanceResponse = response.json();
        assert_eq!(balance.user_id, user.id);
        assert_eq!(balance.current_balance, Decimal::ZERO);

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "100.50").await;

        // Check balance again
        let response = app
            .get("/admin/api/v1/users/current/credits/balance")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserBalanceResponse = response.json();
        assert_eq!(balance.current_balance, Decimal::from_str("100.50").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_balance_unauthenticated(pool: PgPool) {
        let (app, _) = create_test_app(pool, false).await;

        let response = app.get("/admin/api/v1/users/current/credits/balance").await;
        response.assert_status_unauthorized();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_current_user_transactions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple transactions (each adds to the previous balance)
        create_initial_credit_transaction(&pool, user.id, "100.0").await;
        create_initial_credit_transaction(&pool, user.id, "50.0").await;
        create_initial_credit_transaction(&pool, user.id, "25.0").await;

        // List all transactions
        let response = app
            .get("/admin/api/v1/users/current/credits/transactions")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 3);

        // Should be ordered by created_at DESC (most recent first)
        // The balances should be: 175 (latest), 150, 100
        assert_eq!(transactions[0].balance_after, Decimal::from_str("175.0").unwrap());
        assert_eq!(transactions[1].balance_after, Decimal::from_str("150.0").unwrap());
        assert_eq!(transactions[2].balance_after, Decimal::from_str("100.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_current_user_transactions_pagination(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple transactions
        for i in 1..=5 {
            create_initial_credit_transaction(&pool, user.id, &format!("{}.0", i * 10)).await;
        }

        // Test with limit
        let response = app
            .get("/admin/api/v1/users/current/credits/transactions?limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 2);

        // Test with skip
        let response = app
            .get("/admin/api/v1/users/current/credits/transactions?skip=2&limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 2);

        // Test with skip beyond available
        let response = app
            .get("/admin/api/v1/users/current/credits/transactions?skip=100")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert!(transactions.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_admin_grant(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let transaction = json!({
            "transaction_type": "admin_grant",
            "amount": "100.50",
            "description": "Initial credit grant"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let created_transaction: CreditTransactionResponse = response.json();
        assert_eq!(created_transaction.user_id, user.id);
        assert_eq!(created_transaction.amount, Decimal::from_str("100.50").unwrap());
        assert_eq!(created_transaction.balance_after, Decimal::from_str("100.50").unwrap());
        assert_eq!(created_transaction.description, Some("Initial credit grant".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_admin_removal(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // First add credits
        create_initial_credit_transaction(&pool, user.id, "100.0").await;

        // Then remove some
        let transaction = json!({
            "transaction_type": "admin_removal",
            "amount": "30.0",
            "description": "Credit adjustment"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let created_transaction: CreditTransactionResponse = response.json();
        assert_eq!(created_transaction.balance_after, Decimal::from_str("70.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_insufficient_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add 50 credits
        create_initial_credit_transaction(&pool, user.id, "50.0").await;

        // Try to remove more than available
        let transaction = json!({
            "transaction_type": "admin_removal",
            "amount": "100.0",
            "description": "Too much removal"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_invalid_transaction_type(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try to use 'usage' transaction type (not allowed via API)
        let transaction = json!({
            "transaction_type": "usage",
            "amount": "10.0",
            "description": "Invalid type"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_zero_or_negative_amount(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Try zero amount
        let transaction = json!({
            "transaction_type": "admin_grant",
            "amount": "0",
            "description": "Zero amount"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status_bad_request();

        // Try negative amount
        let transaction = json!({
            "transaction_type": "admin_grant",
            "amount": "-10.0",
            "description": "Negative amount"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .json(&transaction)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_credits_forbidden_for_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let transaction = json!({
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Unauthorized attempt"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", other_user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&transaction)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_balance_as_billing_manager(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add credits to user
        create_initial_credit_transaction(&pool, user.id, "250.75").await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/credits/balance", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let balance: UserBalanceResponse = response.json();
        assert_eq!(balance.user_id, user.id);
        assert_eq!(balance.current_balance, Decimal::from_str("250.75").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_balance_forbidden_for_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/credits/balance", other_user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_as_billing_manager(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple transactions for user
        for i in 1..=3 {
            create_initial_credit_transaction(&pool, user.id, &format!("{}.0", i * 10)).await;
        }

        let response = app
            .get(&format!("/admin/api/v1/users/{}/credits/transactions", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 3);
        assert_eq!(transactions[0].user_id, user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_user_balances(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;

        // Create multiple users with different balances
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let user3 = create_test_user(&pool, Role::StandardUser).await;

        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;
        create_initial_credit_transaction(&pool, user3.id, "300.0").await;

        let response = app
            .get("/admin/api/v1/credits/balances")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let balances: Vec<UserBalanceResponse> = response.json();

        // Should have at least 3 users with balances
        assert!(balances.len() >= 3);

        // Verify our test users are in the list
        assert!(balances.iter().any(|b| b.user_id == user1.id && b.current_balance == Decimal::from_str("100.0").unwrap()));
        assert!(balances.iter().any(|b| b.user_id == user2.id && b.current_balance == Decimal::from_str("200.0").unwrap()));
        assert!(balances.iter().any(|b| b.user_id == user3.id && b.current_balance == Decimal::from_str("300.0").unwrap()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_user_balances_forbidden_for_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/credits/balances")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;

        // Create multiple users with transactions
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user1.id, "50.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;

        let response = app
            .get("/admin/api/v1/credits/transactions")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();

        // Should have at least our 3 transactions
        assert!(transactions.len() >= 3);

        // Verify transactions from both users are present
        assert!(transactions.iter().any(|t| t.user_id == user1.id));
        assert!(transactions.iter().any(|t| t.user_id == user2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions_pagination(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_billing_manager_user(&pool).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple transactions
        for i in 1..=5 {
            create_initial_credit_transaction(&pool, user.id, &format!("{}.0", i * 10)).await;
        }

        // Test with limit
        let response = app
            .get("/admin/api/v1/credits/transactions?limit=2")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert_eq!(transactions.len(), 2);

        // Test with skip
        let response = app
            .get("/admin/api/v1/credits/transactions?skip=2&limit=2")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let transactions: Vec<CreditTransactionResponse> = response.json();
        assert!(transactions.len() <= 2);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_user_has_all_credit_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::StandardUser).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Admin should be able to grant credits even without BillingManager role
        let transaction = json!({
            "transaction_type": "admin_grant",
            "amount": "100.0",
            "description": "Admin grant"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/credits", user.id))
            .add_header(add_auth_headers(&admin).0, add_auth_headers(&admin).1)
            .json(&transaction)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Admin should be able to view all balances
        let response = app
            .get("/admin/api/v1/credits/balances")
            .add_header(add_auth_headers(&admin).0, add_auth_headers(&admin).1)
            .await;

        response.assert_status_ok();
    }
}
