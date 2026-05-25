//! Underway job for asynchronous email sends.
//!
//! Every public email send goes through `SendEmailJob`. The handler builds
//! a [`SendEmailInput`] and enqueues it; the worker picks it up and runs the
//! actual send against the configured transport.
//!
//! Enqueueing decouples the user-facing request from the upstream send: a
//! provider error no longer fails the request, and the worker retries
//! transient errors (HTTP 5xx, 429, network) with backoff. Permanent errors
//! (other 4xx) fail the job immediately.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::email::EmailService;
use crate::notifications::BatchNotificationInfo;

/// One enqueued email send. The variant determines which template is rendered
/// and which fields are required; the worker matches on the variant and
/// dispatches to the matching `EmailService::send_*` method.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SendEmailInput {
    PasswordReset {
        to_email: String,
        to_name: Option<String>,
        token_id: Uuid,
        token: String,
    },
    BatchCompletion {
        to_email: String,
        to_name: Option<String>,
        info: BatchNotificationInfo,
        first_batch: bool,
    },
    LowBalance {
        to_email: String,
        to_name: Option<String>,
        balance: Decimal,
    },
    AutoTopupSuccess {
        to_email: String,
        to_name: Option<String>,
        amount: Decimal,
        threshold: Decimal,
        new_balance: Decimal,
    },
    AutoTopupFailed {
        to_email: String,
        to_name: Option<String>,
        amount: Decimal,
        threshold: Decimal,
    },
    AutoTopupLimitReached {
        to_email: String,
        to_name: Option<String>,
        monthly_limit: Decimal,
        balance: Decimal,
    },
    OrgInvite {
        to_email: String,
        org_name: String,
        inviter_name: String,
        role: String,
        invite_link: String,
    },
    SupportRequest {
        support_email: String,
        user_email: String,
        user_name: Option<String>,
        subject: String,
        message: String,
    },
    OrgEmailChangeVerifyNew {
        to_email: String,
        org_name: String,
        confirm_link: String,
    },
    OrgEmailChangeVerifyOld {
        to_email: String,
        org_name: String,
        new_email: String,
        confirm_link: String,
        support_email: Option<String>,
    },
}

impl SendEmailInput {
    /// Stable identifier for logs / metrics. Used to keep tracing readable
    /// without dumping the whole payload (which may contain a password-reset
    /// token or other sensitive material).
    fn kind_label(&self) -> &'static str {
        match self {
            Self::PasswordReset { .. } => "password_reset",
            Self::BatchCompletion { first_batch: true, .. } => "first_batch",
            Self::BatchCompletion { first_batch: false, .. } => "batch_complete",
            Self::LowBalance { .. } => "low_balance",
            Self::AutoTopupSuccess { .. } => "auto_topup_success",
            Self::AutoTopupFailed { .. } => "auto_topup_failed",
            Self::AutoTopupLimitReached { .. } => "auto_topup_limit_reached",
            Self::OrgInvite { .. } => "org_invite",
            Self::SupportRequest { .. } => "support_request",
            Self::OrgEmailChangeVerifyNew { .. } => "org_email_change_verify_new",
            Self::OrgEmailChangeVerifyOld { .. } => "org_email_change_verify_old",
        }
    }
}

/// Build the underway job that sends queued emails. The step closure
/// constructs an [`EmailService`] from the current shared config (so live
/// config edits take effect on the next job) and dispatches based on the
/// input variant.
pub async fn build_send_email_job<P>(
    pool: sqlx::PgPool,
    state: crate::tasks::TaskState<P>,
) -> anyhow::Result<underway::Job<SendEmailInput, crate::tasks::TaskState<P>>>
where
    P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static,
{
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<SendEmailInput, _>::builder()
        .state(state)
        .step(|cx, input: SendEmailInput| async move {
            let kind = input.kind_label();
            let config = cx.state.config.snapshot();

            // Construct fresh each call so transport config edits (e.g. flipping
            // SMTP_PROVIDER) take effect on the next send without restarting.
            let svc = match EmailService::new(&config) {
                Ok(s) => s,
                Err(e) => {
                    // Misconfiguration is permanent — retrying won't help.
                    tracing::error!(error = %e, kind, "EmailService construction failed; dropping send");
                    return To::done();
                }
            };

            let result = dispatch(&svc, input).await;

            match result {
                Ok(()) => {
                    tracing::debug!(kind, "email sent");
                    To::done()
                }
                Err(e) => {
                    // EmailService::send_* always maps provider errors to
                    // `Error::Internal`. We treat them as retryable; underway
                    // applies its built-in backoff. Permanent provider errors
                    // (validation, unverified sender) will keep failing the
                    // same way and eventually exhaust retries — that's the
                    // right behavior: a permanent provider error means the
                    // config or recipient is wrong and needs human attention,
                    // and the retries give time to fix it before the job is
                    // marked dead.
                    tracing::warn!(error = %e, kind, "email send failed; will retry");
                    Err(TaskError::Retryable(e.to_string()))
                }
            }
        })
        .name("send-email")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

/// Dispatch one [`SendEmailInput`] to the matching [`EmailService`] method.
///
/// Lifted to a free function so [`build_send_email_job`]'s step closure stays
/// short and `match`-readable.
async fn dispatch(svc: &EmailService, input: SendEmailInput) -> Result<(), crate::errors::Error> {
    match input {
        SendEmailInput::PasswordReset {
            to_email,
            to_name,
            token_id,
            token,
        } => {
            svc.send_password_reset_email(&to_email, to_name.as_deref(), &token_id, &token)
                .await
        }
        SendEmailInput::BatchCompletion {
            to_email,
            to_name,
            info,
            first_batch,
        } => {
            svc.send_batch_completion_email(&to_email, to_name.as_deref(), &info, first_batch)
                .await
        }
        SendEmailInput::LowBalance {
            to_email,
            to_name,
            balance,
        } => svc.send_low_balance_email(&to_email, to_name.as_deref(), &balance).await,
        SendEmailInput::AutoTopupSuccess {
            to_email,
            to_name,
            amount,
            threshold,
            new_balance,
        } => {
            svc.send_auto_topup_success_email(&to_email, to_name.as_deref(), &amount, &threshold, &new_balance)
                .await
        }
        SendEmailInput::AutoTopupFailed {
            to_email,
            to_name,
            amount,
            threshold,
        } => {
            svc.send_auto_topup_failed_email(&to_email, to_name.as_deref(), &amount, &threshold)
                .await
        }
        SendEmailInput::AutoTopupLimitReached {
            to_email,
            to_name,
            monthly_limit,
            balance,
        } => {
            svc.send_auto_topup_limit_reached_email(&to_email, to_name.as_deref(), &monthly_limit, &balance)
                .await
        }
        SendEmailInput::OrgInvite {
            to_email,
            org_name,
            inviter_name,
            role,
            invite_link,
        } => {
            svc.send_org_invite_email(&to_email, &org_name, &inviter_name, &role, &invite_link)
                .await
        }
        SendEmailInput::SupportRequest {
            support_email,
            user_email,
            user_name,
            subject,
            message,
        } => {
            svc.send_support_request(&support_email, &user_email, user_name.as_deref(), &subject, &message)
                .await
        }
        SendEmailInput::OrgEmailChangeVerifyNew {
            to_email,
            org_name,
            confirm_link,
        } => svc.send_org_email_change_verify_new(&to_email, &org_name, &confirm_link).await,
        SendEmailInput::OrgEmailChangeVerifyOld {
            to_email,
            org_name,
            new_email,
            confirm_link,
            support_email,
        } => {
            svc.send_org_email_change_verify_old(&to_email, &org_name, &new_email, &confirm_link, support_email.as_deref())
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_match_dwctl_email_send_total_template_label() {
        // Keep `SendEmailInput::kind_label` aligned with the `template` label
        // emitted by `EmailService::dispatch` in email.rs. If you add a new
        // variant, mirror it on both sides — the dashboard panels and alerts
        // are wired up to the exact strings.
        use rust_decimal::Decimal;
        use uuid::Uuid;

        let cases: Vec<(SendEmailInput, &'static str)> = vec![
            (
                SendEmailInput::PasswordReset {
                    to_email: String::new(),
                    to_name: None,
                    token_id: Uuid::nil(),
                    token: String::new(),
                },
                "password_reset",
            ),
            (
                SendEmailInput::LowBalance {
                    to_email: String::new(),
                    to_name: None,
                    balance: Decimal::ZERO,
                },
                "low_balance",
            ),
            (
                SendEmailInput::SupportRequest {
                    support_email: String::new(),
                    user_email: String::new(),
                    user_name: None,
                    subject: String::new(),
                    message: String::new(),
                },
                "support_request",
            ),
            (
                SendEmailInput::OrgEmailChangeVerifyOld {
                    to_email: String::new(),
                    org_name: String::new(),
                    new_email: String::new(),
                    confirm_link: String::new(),
                    support_email: None,
                },
                "org_email_change_verify_old",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(input.kind_label(), expected);
        }
    }
}
