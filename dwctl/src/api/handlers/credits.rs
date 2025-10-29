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
