//! HTTP handlers for payment processing endpoints.

use axum::{
    body::Body,
    extract::{FromRequest, State},
    http::{header, Request, StatusCode},
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
use stripe::{CheckoutSession, CheckoutSessionMode, Client, CreateCheckoutSession, CreateCheckoutSessionLineItems, CreateCheckoutSessionAutomaticTax, CheckoutSessionUiMode, CheckoutSessionCustomerCreation, CustomerId, CheckoutSessionPaymentStatus, CheckoutSessionId, Webhook, Event, EventObject, EventType};
use stripe::EventType::{CheckoutSessionAsyncPaymentSucceeded, CheckoutSessionCompleted};

/// Create a Stripe checkout session
/// If no customer ID exists, Stripe creates one automatically and we save it.
/// Webhooks should handle payment completion and balance updates.
async fn create_stripe_checkout_session(
    db_pool: &PgPool,
    user: &CurrentUser,
    api_key: &str,
    price_id: &str,
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
                price: Some(price_id.to_string()),
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

/// StripeEvent extractor that validates webhook signatures
struct StripeEvent(Event);

impl FromRequest<AppState> for StripeEvent
where
    String: FromRequest<AppState>,
{
    type Rejection = Response;

    async fn from_request(req: Request<Body>, state: &AppState) -> Result<Self, Self::Rejection> {
        let signature = if let Some(sig) = req.headers().get("stripe-signature") {
            sig.to_owned()
        } else {
            tracing::error!("Missing stripe-signature header");
            return Err(StatusCode::BAD_REQUEST.into_response());
        };

        let payload =
            String::from_request(req, state).await.map_err(IntoResponse::into_response)?;

        // Get webhook secret from config
        let webhook_secret = match state.config.payment.as_ref() {
            Some(crate::config::PaymentConfig::Stripe(stripe_config)) => {
                &stripe_config.webhook_secret
            }
            None => {
                tracing::error!("Payment provider not configured");
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };

        Ok(Self(
            Webhook::construct_event(&payload, signature.to_str().unwrap(), webhook_secret)
                .map_err(|e| {
                    tracing::error!("Failed to construct webhook event: {:?}", e);
                    StatusCode::BAD_REQUEST.into_response()
                })?,
        ))
    }
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
    headers: axum::http::HeaderMap,
    user: CurrentUser,
) -> Result<Response, StatusCode> {
    // Get Stripe config
    let stripe_config = match state.config.payment.as_ref() {
        Some(crate::config::PaymentConfig::Stripe(stripe_config)) => {
            stripe_config
        }
        None => {
            tracing::warn!("Checkout requested but no payment provider is configured");
            let error_response = Json(json!({
                "error": "No payment provider configured",
                "message": "Sorry, there's no payment provider setup. Please contact support."
            }));
            return Ok((StatusCode::NOT_IMPLEMENTED, error_response).into_response());
        }
    };

    // Build redirect URLs from request origin
    let origin = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| {
            // If it's a referer, extract just the origin part
            if let Ok(url) = url::Url::parse(s) {
                url.origin().ascii_serialization().into()
            } else {
                Some(s.to_string())
            }
        })
        .unwrap_or_else(|| {
            // Fallback to constructing from Host header
            let host = headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("localhost:3001");

            // Determine protocol - check X-Forwarded-Proto for proxied requests
            let proto = headers
                .get("x-forwarded-proto")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("http");

            format!("{}://{}", proto, host)
        });

    let success_url = format!("{}/cost-management?payment=success", origin);
    let cancel_url = format!("{}/cost-management?payment=cancelled", origin);

    tracing::info!("Building checkout URLs with origin: {}", origin);

    // Create checkout session and get checkout URL
    let checkout_url = create_stripe_checkout_session(
        &state.db,
        &user,
        &stripe_config.api_key,
        &stripe_config.price_id,
        &cancel_url,
        &success_url,
    )
    .await?;

    // Return the checkout URL as JSON for the frontend to navigate to
    Ok(Json(json!({
        "url": checkout_url
    })).into_response())
}

/// Stripe webhook handler
/// Receives webhook events from Stripe and processes them
/// This function is idempotent - if a transaction with the same source_id (session_id) already exists,
/// it will not create a duplicate transaction.
#[tracing::instrument(skip_all)]
pub async fn stripe_webhook(
    State(state): State<AppState>,
    StripeEvent(event): StripeEvent,
) -> StatusCode {
    use crate::db::{
        handlers::credits::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    };
    use rust_decimal::Decimal;

    tracing::info!("Received webhook event: {:?}", event.type_);

    match event.type_ {
        EventType::CheckoutSessionCompleted | EventType::CheckoutSessionAsyncPaymentSucceeded => {
            // Extract the session from the event object
            let session = match event.data.object {
                EventObject::CheckoutSession(session) => session,
                _ => {
                    tracing::error!("Expected CheckoutSession object, got something else");
                    return StatusCode::OK;
                }
            };

            tracing::info!(
                "Processing checkout session event for session: {:?}",
                session.id
            );

            let session_id = &session.id;

            // Check if a transaction with this session_id already exists (idempotency check)
            let existing = match sqlx::query!(
                r#"
                SELECT id FROM credits_transactions
                WHERE source_id = $1
                LIMIT 1
                "#,
                session_id.as_str()
            )
            .fetch_optional(&state.db)
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!("Failed to check for existing transaction: {:?}", e);
                    return StatusCode::OK; // Return 200 to prevent Stripe retries
                }
            };

            if existing.is_some() {
                tracing::info!("Transaction for session_id {} already exists, skipping (idempotent)", session_id);
                return StatusCode::OK;
            }

            // Get Stripe API key from config
            let api_key = match state.config.payment.as_ref() {
                Some(crate::config::PaymentConfig::Stripe(stripe_config)) => {
                    &stripe_config.api_key
                }
                None => {
                    tracing::error!("Stripe not configured");
                    return StatusCode::OK;
                }
            };
            let client = Client::new(api_key);

            // Retrieve full checkout session with line items
            let checkout_session = match CheckoutSession::retrieve(&client, session_id, &*["line_items"]).await {
                Ok(session) => session,
                Err(e) => {
                    tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                    return StatusCode::OK; // Return 200 to prevent Stripe retries
                }
            };

            // Extract user ID from client_reference_id
            let local_user_id = match checkout_session.client_reference_id {
                Some(ref id) => id,
                None => {
                    tracing::error!("Checkout session missing client_reference_id");
                    return StatusCode::OK;
                }
            };

            // Verify payment status
            if checkout_session.payment_status != CheckoutSessionPaymentStatus::Paid {
                tracing::info!(
                    "Transaction for session_id {} has not been paid (status: {:?}), skipping.",
                    session_id,
                    checkout_session.payment_status
                );
                return StatusCode::OK;
            }

            // Get price from line_items or amount_total
            let price = match checkout_session
                .line_items
                .and_then(|items| items.data.first().map(|item| item.amount_total))
                .or(checkout_session.amount_total)
            {
                Some(amount) => amount,
                None => {
                    tracing::error!("Checkout session missing both line_items and amount_total");
                    return StatusCode::OK;
                }
            };

            // Create the credit transaction
            let mut conn = match state.db.acquire().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::error!("Failed to acquire database connection: {:?}", e);
                    return StatusCode::OK;
                }
            };

            let mut credits = Credits::new(&mut conn);

            let request = CreditTransactionCreateDBRequest {
                user_id: match local_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Failed to parse user ID: {:?}", e);
                        return StatusCode::OK;
                    }
                },
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from(price),
                source_id: session_id.to_string(),
                description: Some(format!("Stripe payment ({})", session_id)),
            };

            if let Err(e) = credits.create_transaction(&request).await {
                tracing::error!("Failed to create credit transaction: {:?}", e);
                return StatusCode::OK;
            }

            tracing::info!("Successfully fulfilled checkout session {} for user {}", session_id, local_user_id);
        }
        _ => {
            tracing::debug!("Ignoring webhook event type: {:?}", event.type_);
        }
    }

    StatusCode::OK
}
