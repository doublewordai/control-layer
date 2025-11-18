//! HTTP handlers for payment processing endpoints.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use sqlx::PgPool;

use crate::{
    api::models::users::CurrentUser,
    types::UserId,
    AppState,
};
use stripe::{
    CheckoutSession, CheckoutSessionMode, Client, CreateCheckoutSession,
    CreateCheckoutSessionLineItems, CreateCheckoutSessionAutomaticTax,
    CheckoutSessionUiMode, CheckoutSessionCustomerCreation, CustomerId,
    CheckoutSessionPaymentStatus, CheckoutSessionId,
};

/// Create a Stripe checkout session
/// If no customer ID exists, Stripe creates one automatically and we save it.
/// Webhooks should handle payment completion and balance updates.
async fn create_stripe_checkout_session(
    db_pool: &PgPool,
    user: &CurrentUser,
    api_key: &str,
    cancel_url: &str,
    success_url: &str,
) -> Result<String, StatusCode> {
    let client = Client::new(api_key);

    // Build checkout session parameters
    let mut checkout_params = CreateCheckoutSession {
        cancel_url: Some(cancel_url),
        success_url: Some(success_url),
        client_reference_id: Some(&user.id.to_string()),
        currency: Some(stripe::Currency::USD),
        line_items: Some(vec![
            CreateCheckoutSessionLineItems {
                price: Some("price_1SUSd1GdjfBnc3h7uHVkmhGg".to_string()),
                quantity: Some(1),
                ..Default::default()
            }
        ]),
        automatic_tax: Some(CreateCheckoutSessionAutomaticTax {
            enabled: true,
            ..Default::default()
        }),
        mode: Some(CheckoutSessionMode::Payment),
        ui_mode: Some(CheckoutSessionUiMode::Hosted),
        customer_creation: Some(CheckoutSessionCustomerCreation::Always),
        expand: &["line_items"],
        ..Default::default()
    };

    // Include existing customer ID if we have one
    if let Some(existing_id) = &user.payment_provider_id {
        tracing::info!("Using existing Stripe customer ID {} for user {}", existing_id, user.id);
        checkout_params.customer = Some(CustomerId::from(existing_id.parse().unwrap()));
    } else {
        tracing::info!("No customer ID found for user {}, Stripe will create one", user.id);
        // Provide customer email for the new customer
        checkout_params.customer_email = Some(&user.email);
    }

    // Create checkout session
    let checkout_session = CheckoutSession::create(&client, checkout_params)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tracing::info!("Created checkout session {} for user {}", checkout_session.id, user.id);

    // If we didn't have a customer ID before, save the newly created one
    if user.payment_provider_id.is_none() {
        if let Some(customer) = &checkout_session.customer {
            let customer_id = customer.id().to_string();
            tracing::info!("Saving newly created customer ID {} for user {}", customer_id, user.id);

            sqlx::query!(
                "UPDATE users SET payment_provider_id = $1 WHERE id = $2",
                customer_id,
                user.id
            )
            .execute(db_pool)
            .await
            .map_err(|e| {
                tracing::error!("Failed to update user payment_provider_id: {:?}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }
    }

    // Return checkout URL for hosted checkout
    checkout_session
        .url
        .ok_or_else(|| {
            tracing::error!("Checkout session missing URL");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Fulfill a Stripe checkout session by adding a crediting transaction
/// This function is idempotent - if a transaction with the same source_id (session_id) already exists,
/// it will not create a duplicate transaction.
async fn fulfill_stripe_checkout_session(
    db_pool: &PgPool,
    session_id: CheckoutSessionId,
    api_key: String,
) -> Result<(), StatusCode> {
    use crate::db::{
        handlers::credits::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    };
    use rust_decimal::Decimal;
    let client = Client::new(api_key);


    // Check if a transaction with this session_id already exists (idempotency check)
    let existing = sqlx::query!(
        r#"
        SELECT id FROM credits_transactions
        WHERE source_id = $1
        LIMIT 1
        "#,
        &session_id.as_str()
    )
    .fetch_optional(db_pool)
    .await
    .map_err(|e| {
        tracing::error!("Failed to check for existing transaction: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if existing.is_some() {
        tracing::info!("Transaction for session_id {} already exists, skipping (idempotent)", session_id);
        return Ok(());
    }

    // Retrieve checkout session
    let checkout_session = CheckoutSession::retrieve(&client, &session_id, &*["line_items"])
        .await
        .map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let local_user_id = checkout_session
        .client_reference_id
        .ok_or_else(|| {
            tracing::error!("Checkout session missing client_reference_id");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;


    if checkout_session.payment_status != CheckoutSessionPaymentStatus::Paid {
        tracing::info!("Transaction for session_id {} has not been paid (status: {:?}), skipping.", session_id, checkout_session.payment_status);
        return Ok(());
    }

    // Else transaction should be credited
    // Try to get price from line_items[0], fallback to session amount_total
    let price = checkout_session
        .line_items
        .and_then(|items| items.data.first().map(|item| item.amount_total))
        .or(checkout_session.amount_total)
        .ok_or_else(|| {
            tracing::error!("Checkout session missing both line_items and amount_total");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;


    // Create the credit transaction
    let mut conn = db_pool.acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut credits = Credits::new(&mut conn);

    let request = CreditTransactionCreateDBRequest {
        user_id: local_user_id.parse().unwrap(),
        transaction_type: CreditTransactionType::Purchase,
        amount: Decimal::from(price),
        source_id: session_id.to_string(),
        description: Some(format!("Stripe payment ({})", session_id)),
    };

    credits.create_transaction(&request).await.map_err(|e| {
        tracing::error!("Failed to create credit transaction: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!("Successfully fulfilled checkout session {} for user {}", session_id, local_user_id);
    Ok(())
}



#[utoipa::path(
    post,
    path = "/create_checkout",
    tag = "payments",
    summary = "Create checkout session",
    description = "Creates a checkout session and redirects to the payment provider",
    responses(
        (status = 303, description = "Redirect to payment provider checkout page"),
        (status = 501, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_checkout(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<Response, StatusCode> {
    // Check if payment processor is configured
    let processor = state
        .config
        .payment_processor
        .as_deref();

    // Only Stripe is supported for now
    if processor != Some("stripe") {
        tracing::warn!("Checkout requested but no payment provider is configured");
        let error_response = Json(json!({
            "error": "No payment provider configured",
            "message": "Sorry, there's no payment provider setup. Please contact support."
        }));
        return Ok((StatusCode::NOT_IMPLEMENTED, error_response).into_response());
    }

    // // Get Stripe API key
    // let api_key = state
    //     .config
    //     .metadata
    //     .stripe_api_key
    //     .as_ref()
    //     .ok_or_else(|| {
    //         tracing::error!("Stripe API key not configured");
    //         StatusCode::INTERNAL_SERVER_ERROR
    //     })?;
    let api_key = "key";

    // Create checkout session and get checkout URL
    let checkout_url = create_stripe_checkout_session(
        &state.db,
        &user,
        api_key,
        "http://test.com/cancel",
        "http://test.com/success",
    )
    .await?;

    // Return the checkout URL as JSON for the frontend to navigate to
    Ok(Json(json!({
        "url": checkout_url
    })).into_response())
}

/// Stripe webhook handler
/// Receives webhook events from Stripe and processes them
#[tracing::instrument(skip_all)]
pub async fn stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<StatusCode, StatusCode> {
    // Get the webhook secret from config
    // TODO: Move this to config
    let webhook_secret = "whsec_..."; // Replace with actual webhook secret

    // Get the Stripe signature header
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            tracing::error!("Missing stripe-signature header");
            StatusCode::BAD_REQUEST
        })?;

    // Parse the JSON payload
    let event: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        tracing::error!("Failed to parse webhook JSON: {:?}", e);
        StatusCode::BAD_REQUEST
    })?;

    // TODO: Verify the webhook signature using stripe::Webhook::construct_event
    // For now, we'll skip signature verification (NOT PRODUCTION READY)
    tracing::warn!("Webhook signature verification not yet implemented - signature: {}", signature);

    // Extract event type and data
    let event_type = event["type"].as_str().ok_or_else(|| {
        tracing::error!("Missing event type in webhook payload");
        StatusCode::BAD_REQUEST
    })?;

    tracing::info!("Received webhook event: {}", event_type);

    // Process checkout.session.completed and checkout.session.async_payment_succeeded
    if event_type == "checkout.session.completed" || event_type == "checkout.session.async_payment_succeeded" {
        let session_id_str = event["data"]["object"]["id"].as_str().ok_or_else(|| {
            tracing::error!("Missing session ID in webhook payload");
            StatusCode::BAD_REQUEST
        })?;

        let session_id = session_id_str.parse::<CheckoutSessionId>().map_err(|e| {
            tracing::error!("Invalid session ID format: {:?}", e);
            StatusCode::BAD_REQUEST
        })?;

        tracing::info!("Processing {} for session {}", event_type, session_id);

        // Get Stripe API key
        let api_key = "key".to_string();

        // Fulfill the checkout session (idempotent)
        if let Err(e) = fulfill_stripe_checkout_session(&state.db, session_id, api_key).await {
            tracing::error!("Failed to fulfill checkout session: {:?}", e);
            // Return 200 anyway to acknowledge receipt and prevent Stripe from retrying
            // The idempotency check ensures we won't double-charge
        }
    } else {
        tracing::debug!("Ignoring webhook event type: {}", event_type);
    }

    Ok(StatusCode::OK)
}
