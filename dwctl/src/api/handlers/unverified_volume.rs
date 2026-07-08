//! Unverified upload-volume enforcement.
//!
//! Unverified creditors — those who have never moved real money, see
//! `users.verified` — get full platform throughput, but only a bounded *volume*
//! of queued work until they verify (add a payment method / make a payment).
//! This protects the system from an unverified user dumping more work than can
//! drain, without throttling throughput (which would feel slow/unreliable).
//!
//! The cap scales with the completion window:
//! `batches.unverified_requests_per_completion_hour * window_hours`, measured
//! over a rolling window equal to the completion window (e.g. 1000 over a
//! trailing 1h for async, 24000 over a trailing 24h for batch). Verified
//! creditors are never limited; a per-hour value of 0 disables the cap.

use fusillade::Storage;

use crate::errors::{Error, Result};
use crate::types::UserId;

use super::sla_capacity::parse_window_to_seconds;

/// Which submission path is being checked — selects the backing count query.
#[derive(Clone, Copy)]
pub enum SubmissionKind {
    /// Bulk batch submission; counts the creditor's batch requests in the window.
    Batch,
    /// Flex/async single request; counts the creditor's batchless flex requests.
    Flex,
}

/// Enforce the unverified upload-volume cap for a submission of `requested`
/// requests with `completion_window`, attributed to `owner` (the creditor: the
/// active organization for org members, otherwise the user), whose verification
/// status is `owner_verified`.
///
/// The caller passes `owner_verified` rather than having this function look it
/// up: both submission paths already resolve the creditor from the API key /
/// session, so the verified flag rides along on that existing lookup (`owner` is
/// `api_keys.user_id`) instead of costing an extra round trip on the hot path.
///
/// Returns `Ok(())` when allowed and `Err(Error::TooManyRequests)` (HTTP 429,
/// with an actionable message) when the rolling-window count plus `requested`
/// would exceed the cap. No-op when the cap is disabled (`per_hour == 0`) or the
/// creditor is verified — the verified short-circuit runs before the count query.
pub async fn enforce_unverified_volume_limit<S: Storage>(
    request_manager: &S,
    per_hour: usize,
    owner: UserId,
    owner_verified: bool,
    completion_window: &str,
    requested: i64,
    kind: SubmissionKind,
) -> Result<()> {
    if per_hour == 0 {
        return Ok(());
    }

    // Verified creditors are never limited. Check this before the count query
    // so verified users (the paying majority) never pay for it.
    if owner_verified {
        return Ok(());
    }

    let window_seconds = parse_window_to_seconds(completion_window);
    // The cap scales with whole hours of the completion window. A sub-hour
    // window with a small per-hour value can floor to 0; treat that as no cap
    // rather than rejecting everything.
    let cap = (per_hour as i64).saturating_mul(window_seconds) / 3600;
    if cap <= 0 {
        return Ok(());
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(window_seconds);
    let owner = owner.to_string();
    // strict = true reads from the write pool so a just-submitted batch/request
    // is reflected immediately (a user rapidly resubmitting must see prior work).
    let current = match kind {
        SubmissionKind::Batch => {
            request_manager
                .sum_owner_batch_requests_in_window(&owner, completion_window, cutoff, true)
                .await
        }
        SubmissionKind::Flex => request_manager.count_owner_flex_requests_since(&owner, cutoff, true).await,
    }
    .map_err(|e| Error::Internal {
        operation: format!("count unverified upload volume: {e}"),
    })?;

    if current + requested > cap {
        return Err(Error::TooManyRequests {
            message: format!(
                "Unverified accounts can submit at most {cap} request(s) per {completion_window} \
                 completion window. You have {current} in the last {completion_window}, so this \
                 submission of {requested} would exceed the limit. Verify your account by adding a \
                 payment method or making a payment to remove this limit."
            ),
        });
    }

    Ok(())
}
