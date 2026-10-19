//! Endpoint-side enforcement of an organization's disabled modalities.
//!
//! The batch surface is gated here, at its two entry points (`POST /files`
//! and `POST /batches`), against the account the work bills to. The realtime
//! surface is gated in [`crate::inference::middleware`] from the per-key
//! policy cache, because that path never resolves a `CurrentUser`.

use sqlx::PgConnection;

use crate::db::handlers::Organizations;
use crate::errors::{Error, Result};
use crate::modalities::Modality;
use crate::types::UserId;

/// Refuse with 403 when `account_id` (the organization in org context, the
/// person otherwise) has the batch API switched off.
pub async fn ensure_batch_enabled(conn: &mut PgConnection, account_id: UserId) -> Result<()> {
    let disabled = Organizations::new(conn).disabled_modalities(account_id).await?;
    if disabled.contains(Modality::Batch) {
        return Err(Error::ModalityDisabled { modality: Modality::Batch });
    }
    Ok(())
}
