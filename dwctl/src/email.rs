//! Email service for sending password reset emails and notifications.
//!
//! Transport selection is config-driven via [`crate::config::EmailTransportConfig`]:
//! `Smtp` (legacy / self-hosted), `File` (development), and `Http` (hosted
//! transactional providers via [`crate::email_http`]). All transports route
//! through [`EmailService::dispatch`], which records the
//! `dwctl_email_send_total{provider, template, outcome}` counter.

use std::path::Path;
use std::sync::Arc;

use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use minijinja::{Environment, context};

use crate::config::HttpProviderConfig;
use crate::email_http::{HttpEmailClient, ResendClient};
use crate::notifications::{BatchNotificationInfo, BatchOutcome};
use crate::{config::Config, errors::Error};

struct EmailTemplates {
    password_reset: String,
    batch_complete: String,
    first_batch: String,
    low_balance: String,
    auto_topup_success: String,
    auto_topup_failed: String,
    auto_topup_limit_reached: String,
    org_invite: String,
    org_email_change_verify_new: String,
    org_email_change_verify_old: String,
}

impl EmailTemplates {
    fn embedded() -> Self {
        Self {
            password_reset: include_str!("../default_templates/password_reset.html").to_string(),
            batch_complete: include_str!("../default_templates/batch_complete.html").to_string(),
            first_batch: include_str!("../default_templates/first_batch.html").to_string(),
            low_balance: include_str!("../default_templates/low_balance.html").to_string(),
            auto_topup_success: include_str!("../default_templates/auto_topup_success.html").to_string(),
            auto_topup_failed: include_str!("../default_templates/auto_topup_failed.html").to_string(),
            auto_topup_limit_reached: include_str!("../default_templates/auto_topup_limit_reached.html").to_string(),
            org_invite: include_str!("../default_templates/org_invite.html").to_string(),
            org_email_change_verify_new: include_str!("../default_templates/org_email_change_verify_new.html").to_string(),
            org_email_change_verify_old: include_str!("../default_templates/org_email_change_verify_old.html").to_string(),
        }
    }

    fn load_from_dir(dir: &Path) -> Self {
        let embedded = Self::embedded();

        let load = |name: &str, fallback: String| -> String {
            let path = dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => {
                    tracing::debug!("Email template {name} not found in custom dir, using embedded default");
                    fallback
                }
            }
        };

        Self {
            password_reset: load("password_reset.html", embedded.password_reset),
            batch_complete: load("batch_complete.html", embedded.batch_complete),
            first_batch: load("first_batch.html", embedded.first_batch),
            low_balance: load("low_balance.html", embedded.low_balance),
            auto_topup_success: load("auto_topup_success.html", embedded.auto_topup_success),
            auto_topup_failed: load("auto_topup_failed.html", embedded.auto_topup_failed),
            auto_topup_limit_reached: load("auto_topup_limit_reached.html", embedded.auto_topup_limit_reached),
            org_invite: load("org_invite.html", embedded.org_invite),
            org_email_change_verify_new: load("org_email_change_verify_new.html", embedded.org_email_change_verify_new),
            org_email_change_verify_old: load("org_email_change_verify_old.html", embedded.org_email_change_verify_old),
        }
    }
}

pub struct EmailService {
    transport: EmailTransport,
    from_email: String,
    from_name: String,
    base_url: String,
    reply_to: Option<String>,
    templates: EmailTemplates,
}

/// Backend used by [`EmailService`] to deliver a built [`EmailEnvelope`].
///
/// `Smtp` and `File` are lettre-backed (existing behavior). `Http` delegates
/// to a provider-specific [`HttpEmailClient`] (e.g. [`ResendClient`]).
enum EmailTransport {
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    File(AsyncFileTransport<Tokio1Executor>),
    Http(Arc<dyn HttpEmailClient>),
}

/// Transport-independent representation of a prepared email.
///
/// Built by the public `send_*` methods on [`EmailService`] and consumed by
/// [`EmailService::dispatch`]. The SMTP/File paths turn this back into a
/// `lettre::Message`; the HTTP path serializes it to the provider's JSON shape.
///
/// `template_id` is a stable identifier (e.g. `"password_reset"`) used as a
/// label on the `dwctl_email_send_total` Prometheus counter.
#[derive(Debug, Clone)]
pub(crate) struct EmailEnvelope {
    pub template_id: &'static str,
    pub from: Mailbox,
    pub to: Mailbox,
    pub reply_to: Option<Mailbox>,
    pub subject: String,
    pub body: EmailBody,
}

/// Body of an email — either HTML or plain text. Determines the
/// `Content-Type` for SMTP/File transports and which field is populated
/// in the provider JSON body for HTTP transports.
#[derive(Debug, Clone)]
pub(crate) enum EmailBody {
    Html(String),
    Text(String),
}

impl EmailService {
    pub fn new(config: &Config) -> Result<Self, Error> {
        let email_config = &config.email;

        let transport = match &email_config.transport {
            crate::config::EmailTransportConfig::Smtp {
                host,
                port,
                username,
                password,
                use_tls,
            } => {
                // Use SMTP transport
                if !use_tls {
                    tracing::warn!("SMTP TLS is disabled - this is not recommended for production");
                }

                let smtp_builder = if *use_tls {
                    AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                } else {
                    Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host))
                }
                .map_err(|e| Error::Internal {
                    operation: format!("create SMTP transport: {e}"),
                })?
                .port(*port)
                .credentials(Credentials::new(username.clone(), password.clone()));

                EmailTransport::Smtp(smtp_builder.build())
            }
            crate::config::EmailTransportConfig::File { path } => {
                // Use file transport for development/testing
                let emails_dir = Path::new(path);
                if !emails_dir.exists() {
                    std::fs::create_dir_all(emails_dir).map_err(|e| Error::Internal {
                        operation: format!("create emails directory: {e}"),
                    })?;
                }
                let file_transport = AsyncFileTransport::<Tokio1Executor>::new(emails_dir);
                EmailTransport::File(file_transport)
            }
            crate::config::EmailTransportConfig::Http { provider } => {
                // Hosted transactional provider over HTTPS. Each provider
                // implements `HttpEmailClient` in `email_http`; add a match
                // arm here when introducing a new one.
                let client: Arc<dyn HttpEmailClient> = match provider {
                    HttpProviderConfig::Resend { api_key, base_url } => {
                        Arc::new(ResendClient::new(api_key.clone(), base_url.clone()).map_err(|e| Error::Internal {
                            operation: format!("create Resend client: {e}"),
                        })?)
                    }
                };
                EmailTransport::Http(client)
            }
        };

        let templates = match &email_config.templates_dir {
            Some(dir) => EmailTemplates::load_from_dir(Path::new(dir)),
            None => EmailTemplates::embedded(),
        };

        Ok(Self {
            transport,
            from_email: email_config.from_email.clone(),
            from_name: email_config.from_name.clone(),
            base_url: config.dashboard_url.clone(),
            reply_to: email_config.reply_to.clone(),
            templates,
        })
    }

    pub async fn send_password_reset_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        token_id: &uuid::Uuid,
        token: &str,
    ) -> Result<(), Error> {
        let reset_link = format!("{}/reset-password?id={}&token={}", self.base_url, token_id, token);

        let subject = "Password Reset Request";
        let name = to_name.unwrap_or("User");
        let body = self.render_password_reset_body(name, &reset_link).map_err(|e| Error::Internal {
            operation: format!("render email template: {e}"),
        })?;

        self.send_email("password_reset", to_email, to_name, subject, &body).await
    }

    /// Build a standard HTML-body envelope for one recipient and dispatch it.
    ///
    /// Used by every template-driven sender. `template_id` is a stable string
    /// (e.g. `"password_reset"`) that becomes the `template` label on the
    /// `dwctl_email_send_total` counter.
    async fn send_email(
        &self,
        template_id: &'static str,
        to_email: &str,
        to_name: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Result<(), Error> {
        let envelope = self.build_envelope(template_id, to_email, to_name, subject, EmailBody::Html(body.to_string()))?;
        self.dispatch(envelope).await
    }

    /// Construct an [`EmailEnvelope`] with this service's `from` / `reply_to`
    /// configuration applied. Shared by `send_email` and `send_support_request`.
    fn build_envelope(
        &self,
        template_id: &'static str,
        to_email: &str,
        to_name: Option<&str>,
        subject: &str,
        body: EmailBody,
    ) -> Result<EmailEnvelope, Error> {
        let from_address = self.from_email.parse().map_err(|e| Error::Internal {
            operation: format!("Failed to parse from email: {e}"),
        })?;
        let from = Mailbox::new(Some(self.from_name.clone()), from_address);

        let to_address = to_email.parse().map_err(|e| Error::Internal {
            operation: format!("Failed to parse to email: {e}"),
        })?;
        let to = Mailbox::new(to_name.map(|n| n.to_string()), to_address);

        let reply_to = self
            .reply_to
            .as_deref()
            .map(|email| -> Result<Mailbox, Error> {
                let addr = email.parse().map_err(|e| Error::Internal {
                    operation: format!("Failed to parse reply-to email: {e}"),
                })?;
                Ok(Mailbox::new(Some(self.from_name.clone()), addr))
            })
            .transpose()?;

        Ok(EmailEnvelope {
            template_id,
            from,
            to,
            reply_to,
            subject: subject.to_string(),
            body,
        })
    }

    /// Dispatch a prepared envelope via the configured transport and record
    /// the `dwctl_email_send_total{provider, template, outcome}` counter.
    ///
    /// `outcome` is `"sent"` on success and `"failed"` on any error.
    async fn dispatch(&self, envelope: EmailEnvelope) -> Result<(), Error> {
        let template_id = envelope.template_id;
        let provider = transport_label(&self.transport);

        let result = self.dispatch_inner(envelope).await;
        let outcome = if result.is_ok() { "sent" } else { "failed" };
        metrics::counter!(
            "dwctl_email_send_total",
            "provider" => provider,
            "template" => template_id,
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    async fn dispatch_inner(&self, envelope: EmailEnvelope) -> Result<(), Error> {
        match &self.transport {
            EmailTransport::Smtp(smtp) => {
                let message = build_lettre_message(&envelope)?;
                smtp.send(message).await.map_err(|e| Error::Internal {
                    operation: format!("send SMTP email: {e}"),
                })?;
            }
            EmailTransport::File(file) => {
                let message = build_lettre_message(&envelope)?;
                file.send(message).await.map_err(|e| Error::Internal {
                    operation: format!("send file email: {e}"),
                })?;
            }
            EmailTransport::Http(client) => {
                client.send(&envelope).await.map_err(|e| {
                    // is_transient is preserved in tracing so callers (worker
                    // contexts in particular) can decide whether to retry.
                    tracing::warn!(
                        provider = client.provider_name(),
                        transient = e.is_transient(),
                        error = %e,
                        "HTTP email send failed",
                    );
                    Error::Internal {
                        operation: format!("send HTTP email via {}: {e}", client.provider_name()),
                    }
                })?;
            }
        }

        Ok(())
    }

    pub async fn send_batch_completion_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        info: &BatchNotificationInfo,
        first_batch: bool,
    ) -> Result<(), Error> {
        let status_text = match info.outcome {
            BatchOutcome::Completed => "completed",
            BatchOutcome::PartiallyCompleted => "completed with errors",
            BatchOutcome::Failed => "failed",
        };
        let subject = if first_batch {
            format!("Your first Doubleword batch has {status_text}")
        } else {
            format!("Batch {} — {}", &info.batch_id[..8.min(info.batch_id.len())], status_text)
        };
        let name = to_name.unwrap_or("User");
        let body = self
            .render_batch_completion_body(name.to_string(), info, first_batch)
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;
        let template_id = if first_batch { "first_batch" } else { "batch_complete" };
        self.send_email(template_id, to_email, to_name, &subject, &body).await
    }

    pub fn render_batch_completion_body(
        &self,
        to_name: String,
        info: &BatchNotificationInfo,
        first_batch: bool,
    ) -> Result<String, minijinja::Error> {
        let template_src = if first_batch {
            &self.templates.first_batch
        } else {
            &self.templates.batch_complete
        };
        let mut env = Environment::new();
        env.add_template("email", template_src)?;

        let (outcome_label, outcome_icon, header_color, outcome_message) = match info.outcome {
            BatchOutcome::Completed => ("Completed", "✓", "#16a34a", "Your batch has finished processing successfully."),
            BatchOutcome::PartiallyCompleted => (
                "Completed with some failures",
                "⚠",
                "#d97706",
                "Your batch has finished processing, but some requests failed.",
            ),
            BatchOutcome::Failed => ("Failed", "✗", "#dc2626", "There was a problem processing your batch."),
        };

        let duration = info
            .finished_at
            .map(|finished| {
                let dur = finished - info.created_at;
                let total_secs = dur.num_seconds();
                if total_secs < 60 {
                    format!("{total_secs}s")
                } else if total_secs < 3600 {
                    format!("{}m {}s", total_secs / 60, total_secs % 60)
                } else {
                    format!("{}h {}m", total_secs / 3600, (total_secs % 3600) / 60)
                }
            })
            .unwrap_or_default();

        let base = self.base_url.trim_end_matches('/');
        let dashboard_link = format!("{base}/batches/{}", info.batch_id);
        let profile_link = format!("{base}/profile");
        let priority = if info.completion_window == "1h" { "Priority" } else { "Standard" };

        env.get_template("email")?.render(context! {
            to_name,
            batch_id => &info.batch_id,
            model => &info.model,
            endpoint => &info.endpoint,
            outcome_label,
            outcome_icon,
            outcome_message,
            header_color,
            created_at => info.created_at.format("%b %d, %Y %H:%M UTC").to_string(),
            finished_at => info.finished_at.map(|t| t.format("%b %d, %Y %H:%M UTC").to_string()).unwrap_or_default(),
            duration,
            completed_requests => info.completed_requests,
            failed_requests => info.failed_requests,
            total_requests => info.total_requests,
            dashboard_link,
            profile_link,
            priority,
            completion_window => &info.completion_window,
            filename => info.filename.as_deref().unwrap_or(""),
            description => info.description.as_deref().unwrap_or(""),
            from_name => &self.from_name,
            reply_to => self.reply_to.as_deref().unwrap_or(&self.from_email),
        })
    }

    fn render_password_reset_body(&self, to_name: &str, reset_link: &str) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", &self.templates.password_reset)?;

        env.get_template("email")?.render(context! {
            to_name,
            reset_link,
        })
    }

    pub async fn send_low_balance_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        balance: &rust_decimal::Decimal,
    ) -> Result<(), Error> {
        let subject = "Your balance is running low";
        let name = to_name.unwrap_or("User");
        let body = self.render_low_balance_body(name, balance).map_err(|e| Error::Internal {
            operation: format!("render email template: {e}"),
        })?;
        self.send_email("low_balance", to_email, to_name, subject, &body).await
    }

    fn render_low_balance_body(&self, to_name: &str, balance: &rust_decimal::Decimal) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", &self.templates.low_balance)?;

        let base = self.base_url.trim_end_matches('/');
        let dashboard_link = format!("{base}/cost-management");
        let profile_link = format!("{base}/profile");

        env.get_template("email")?.render(context! {
            to_name,
            balance => format!("{:.2}", balance),
            dashboard_link,
            profile_link,
            from_name => &self.from_name,
            reply_to => self.reply_to.as_deref().unwrap_or(&self.from_email),
        })
    }

    pub async fn send_auto_topup_success_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        amount: &rust_decimal::Decimal,
        threshold: &rust_decimal::Decimal,
        new_balance: &rust_decimal::Decimal,
    ) -> Result<(), Error> {
        let subject = format!("Auto top-up: ${:.2} added to your account", amount);
        let name = to_name.unwrap_or("User");
        let body = self
            .render_auto_topup_body(&self.templates.auto_topup_success, name, amount, threshold, Some(new_balance))
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;
        self.send_email("auto_topup_success", to_email, to_name, &subject, &body).await
    }

    pub async fn send_auto_topup_failed_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        amount: &rust_decimal::Decimal,
        threshold: &rust_decimal::Decimal,
    ) -> Result<(), Error> {
        let subject = "Auto top-up failed — action required";
        let name = to_name.unwrap_or("User");
        let body = self
            .render_auto_topup_body(&self.templates.auto_topup_failed, name, amount, threshold, None)
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;
        self.send_email("auto_topup_failed", to_email, to_name, subject, &body).await
    }

    pub async fn send_auto_topup_limit_reached_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        monthly_limit: &rust_decimal::Decimal,
        balance: &rust_decimal::Decimal,
    ) -> Result<(), Error> {
        let subject = format!("Auto top-up monthly limit of ${:.2} reached", monthly_limit);
        let name = to_name.unwrap_or("User");

        let mut env = Environment::new();
        env.add_template("email", &self.templates.auto_topup_limit_reached)
            .map_err(|e| Error::Internal {
                operation: format!("add email template: {e}"),
            })?;

        let base = self.base_url.trim_end_matches('/');
        let dashboard_link = format!("{base}/cost-management");
        let profile_link = format!("{base}/profile");

        let body = env
            .get_template("email")
            .map_err(|e| Error::Internal {
                operation: format!("get email template: {e}"),
            })?
            .render(context! {
                to_name => name,
                monthly_limit => format!("{:.2}", monthly_limit),
                balance => format!("{:.2}", balance),
                dashboard_link,
                profile_link,
            })
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;

        self.send_email("auto_topup_limit_reached", to_email, to_name, &subject, &body)
            .await
    }

    fn render_auto_topup_body(
        &self,
        template: &str,
        to_name: &str,
        amount: &rust_decimal::Decimal,
        threshold: &rust_decimal::Decimal,
        new_balance: Option<&rust_decimal::Decimal>,
    ) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", template)?;

        let base = self.base_url.trim_end_matches('/');
        let dashboard_link = format!("{base}/cost-management");
        let profile_link = format!("{base}/profile");

        env.get_template("email")?.render(context! {
            to_name,
            amount => format!("{:.2}", amount),
            threshold => format!("{:.2}", threshold),
            new_balance => new_balance.map(|b| format!("{:.2}", b)).unwrap_or_default(),
            dashboard_link,
            profile_link,
        })
    }

    pub async fn send_org_invite_email(
        &self,
        to_email: &str,
        org_name: &str,
        inviter_name: &str,
        role: &str,
        invite_link: &str,
    ) -> Result<(), Error> {
        let subject = format!("You've been invited to join {org_name}");
        let body = self
            .render_org_invite_body(org_name, inviter_name, role, invite_link)
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;

        self.send_email("org_invite", to_email, None, &subject, &body).await
    }

    /// Send a support request email to the configured support address, with
    /// reply-to set to the user's email so the support team can reply directly.
    ///
    /// Note this overrides the service's default `reply_to`; the address the
    /// support team replies to should always be the requesting user, not the
    /// no-reply sender.
    pub async fn send_support_request(
        &self,
        support_email: &str,
        user_email: &str,
        user_name: Option<&str>,
        subject: &str,
        message: &str,
    ) -> Result<(), Error> {
        let display_name = user_name.unwrap_or(user_email);
        let body = format!("Support request from {} ({}):\n\n{}", display_name, user_email, message);

        // Build the standard envelope, then override reply-to with the user's
        // address so support can reply directly to the requester.
        let mut envelope = self.build_envelope(
            "support_request",
            support_email,
            Some("Doubleword Support"),
            subject,
            EmailBody::Text(body),
        )?;
        let reply_to_address = user_email.parse().map_err(|e| Error::Internal {
            operation: format!("Failed to parse user email for reply-to: {e}"),
        })?;
        envelope.reply_to = Some(Mailbox::new(user_name.map(|n| n.to_string()), reply_to_address));

        self.dispatch(envelope).await
    }

    fn render_org_invite_body(
        &self,
        org_name: &str,
        inviter_name: &str,
        role: &str,
        invite_link: &str,
    ) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", &self.templates.org_invite)?;

        env.get_template("email")?.render(context! {
            org_name,
            inviter_name,
            role,
            invite_link,
        })
    }

    /// Send the verification link to the *new* contact address. The
    /// recipient must click to prove possession of the mailbox; the change
    /// only applies once the old-side has also confirmed.
    pub async fn send_org_email_change_verify_new(&self, to_email: &str, org_name: &str, confirm_link: &str) -> Result<(), Error> {
        let subject = format!("Confirm your new contact email for {org_name}");
        let body = self
            .render_org_email_change_verify_new_body(org_name, confirm_link)
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;

        self.send_email("org_email_change_verify_new", to_email, None, &subject, &body)
            .await
    }

    /// Send the verification link to the *current* contact address — the
    /// legitimate owner must authorize the change before it can take
    /// effect (defends against session-hijack attacks).
    pub async fn send_org_email_change_verify_old(
        &self,
        to_email: &str,
        org_name: &str,
        new_email: &str,
        confirm_link: &str,
        support_email: Option<&str>,
    ) -> Result<(), Error> {
        let subject = format!("Authorize a contact email change for {org_name}");
        let body = self
            .render_org_email_change_verify_old_body(org_name, new_email, confirm_link, support_email)
            .map_err(|e| Error::Internal {
                operation: format!("render email template: {e}"),
            })?;

        self.send_email("org_email_change_verify_old", to_email, None, &subject, &body)
            .await
    }

    fn render_org_email_change_verify_new_body(&self, org_name: &str, confirm_link: &str) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", &self.templates.org_email_change_verify_new)?;

        env.get_template("email")?.render(context! {
            org_name,
            confirm_link,
        })
    }

    fn render_org_email_change_verify_old_body(
        &self,
        org_name: &str,
        new_email: &str,
        confirm_link: &str,
        support_email: Option<&str>,
    ) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", &self.templates.org_email_change_verify_old)?;

        env.get_template("email")?.render(context! {
            org_name,
            new_email,
            confirm_link,
            support_email,
        })
    }
}

/// Static identifier for the configured transport, used as the `provider`
/// label on `dwctl_email_send_total`. SMTP/File use literal strings;
/// HTTP defers to the provider client (e.g. `"resend"`).
fn transport_label(t: &EmailTransport) -> &'static str {
    match t {
        EmailTransport::Smtp(_) => "smtp",
        EmailTransport::File(_) => "file",
        EmailTransport::Http(client) => client.provider_name(),
    }
}

/// Convert a transport-independent [`EmailEnvelope`] into a `lettre::Message`
/// for the SMTP and File transports. The HTTP transport bypasses this and
/// serializes the envelope directly to its provider's wire format.
fn build_lettre_message(envelope: &EmailEnvelope) -> Result<Message, Error> {
    let content_type = match &envelope.body {
        EmailBody::Html(_) => ContentType::TEXT_HTML,
        EmailBody::Text(_) => ContentType::TEXT_PLAIN,
    };

    let mut builder = Message::builder()
        .from(envelope.from.clone())
        .to(envelope.to.clone())
        .subject(&envelope.subject)
        .header(content_type);

    if let Some(reply_to) = &envelope.reply_to {
        builder = builder.reply_to(reply_to.clone());
    }

    let body_str = match &envelope.body {
        EmailBody::Html(s) | EmailBody::Text(s) => s.clone(),
    };

    builder.body(body_str).map_err(|e| Error::Internal {
        operation: format!("build email message: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::utils::create_test_config;

    fn test_info(
        outcome: BatchOutcome,
        total: i64,
        completed: i64,
        failed: i64,
        filename: Option<&str>,
        description: Option<&str>,
    ) -> BatchNotificationInfo {
        BatchNotificationInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            batch_uuid: uuid::Uuid::nil(),
            user_id: uuid::Uuid::nil(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "gpt-4o".to_string(),
            outcome,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: total,
            completed_requests: completed,
            failed_requests: failed,
            cancelled_requests: 0,
            completion_window: "24h".to_string(),
            filename: filename.map(String::from),
            description: description.map(String::from),
            output_file_id: None,
            error_file_id: None,
        }
    }

    #[tokio::test]
    async fn test_email_service_creation() {
        let config = create_test_config();
        let email_service = EmailService::new(&config);
        assert!(email_service.is_ok());
    }

    #[tokio::test]
    async fn test_password_reset_email_body() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let body = email_service
            .render_password_reset_body("John Doe", "https://example.com/reset?token=abc123")
            .unwrap();

        assert!(body.contains("Hello John Doe,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
        assert!(body.contains("Reset your password"));
    }

    #[tokio::test]
    async fn test_password_reset_email_body_no_name() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let body = email_service
            .render_password_reset_body("User", "https://example.com/reset?token=abc123")
            .unwrap();

        assert!(body.contains("Hello User,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
    }

    #[tokio::test]
    async fn test_first_batch_email_body_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = test_info(BatchOutcome::Completed, 50, 50, 0, Some("first-run.jsonl"), None);

        let body = email_service.render_batch_completion_body("Bob".into(), &info, true).unwrap();

        assert!(body.contains("Hi Bob,"));
        assert!(body.contains("first batch has completed"));
        assert!(body.contains("http://localhost:3001/batches/abcd1234-5678-90ab-cdef-1234567890ab"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = test_info(
            BatchOutcome::Completed,
            100,
            100,
            0,
            Some("input.jsonl"),
            Some("Weekly report generation"),
        );

        let body = email_service.render_batch_completion_body("Alice".into(), &info, false).unwrap();

        assert!(body.contains("Hi Alice,"));
        assert!(body.contains("Completed"));
        assert!(body.contains("finished processing successfully"));
        assert!(body.contains("/v1/chat/completions"));
        assert!(body.contains("gpt-4o"));
        assert!(body.contains("100"));
        assert!(body.contains("http://localhost:3001/batches/abcd1234-5678-90ab-cdef-1234567890ab"));
        assert!(body.contains("http://localhost:3001/profile"));
        assert!(body.contains("24h"));
        assert!(body.contains("input.jsonl"));
        assert!(body.contains("Weekly report generation"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_partially_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = test_info(BatchOutcome::PartiallyCompleted, 100, 98, 2, Some("input.jsonl"), None);

        let body = email_service.render_batch_completion_body("Alice".into(), &info, false).unwrap();

        assert!(body.contains("Completed with some failures"));
        assert!(body.contains("some requests failed"));
        assert!(body.contains(">2<"));
    }

    /// Exercises the full send_email path (mailbox construction + message build + file transport)
    /// with various name/email combinations that could trip up RFC 5322 parsing.
    #[tokio::test]
    async fn test_send_email_with_various_recipient_names() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let cases: Vec<(Option<&str>, &str)> = vec![
            // Normal name
            (Some("Alice Smith"), "alice@example.com"),
            // No display name
            (None, "alice@example.com"),
            // Email address as display name (the bug that hit production)
            (Some("josh.cowan@doubleword.ai"), "josh.cowan@doubleword.ai"),
            // Name with special RFC 5322 characters
            (Some("O'Brien, James"), "james@example.com"),
            // Name with parentheses
            (Some("Alice (Engineering)"), "alice@example.com"),
            // Name with quotes
            (Some("Alice \"The Boss\" Smith"), "alice@example.com"),
            // Unicode name
            (Some("Müller, François"), "francois@example.com"),
            // Single word
            (Some("admin"), "admin@example.com"),
            // Empty string display name
            (Some(""), "alice@example.com"),
        ];

        for (name, email) in cases {
            let result = email_service
                .send_email("test_recipients", email, name, "Test Subject", "<p>Hello</p>")
                .await;
            assert!(
                result.is_ok(),
                "send_email failed for name={name:?}, email={email:?}: {:?}",
                result.unwrap_err()
            );
        }
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_failed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = test_info(BatchOutcome::Failed, 100, 0, 100, None, None);

        let body = email_service.render_batch_completion_body("Alice".into(), &info, false).unwrap();

        assert!(body.contains("Failed"));
        assert!(body.contains("problem processing your batch"));
        assert!(body.contains(">100<"));
    }

    #[tokio::test]
    async fn test_auto_topup_success_email_body() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let amount = rust_decimal::Decimal::new(2500, 2); // $25.00
        let threshold = rust_decimal::Decimal::new(500, 2); // $5.00
        let new_balance = rust_decimal::Decimal::new(3000, 2); // $30.00

        let body = email_service
            .render_auto_topup_body(
                &email_service.templates.auto_topup_success,
                "Alice",
                &amount,
                &threshold,
                Some(&new_balance),
            )
            .unwrap();

        assert!(body.contains("Alice"), "Should contain user name");
        assert!(body.contains("25.00"), "Should contain amount");
        assert!(body.contains("5.00"), "Should contain threshold");
        assert!(body.contains("30.00"), "Should contain new balance");
        assert!(body.contains("cost-management"), "Should contain dashboard link");
    }

    #[tokio::test]
    async fn test_auto_topup_failed_email_body() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let amount = rust_decimal::Decimal::new(2500, 2); // $25.00
        let threshold = rust_decimal::Decimal::new(500, 2); // $5.00

        let body = email_service
            .render_auto_topup_body(&email_service.templates.auto_topup_failed, "Bob", &amount, &threshold, None)
            .unwrap();

        assert!(body.contains("Bob"), "Should contain user name");
        assert!(body.contains("25.00"), "Should contain amount");
        assert!(body.contains("5.00"), "Should contain threshold");
        assert!(body.contains("cost-management"), "Should contain dashboard link");
    }

    #[tokio::test]
    async fn test_send_support_request() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let result = email_service
            .send_support_request(
                "support@doubleword.ai",
                "alice@example.com",
                Some("Alice Smith"),
                "Help with API keys",
                "I can't generate a new API key from the dashboard.",
            )
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_support_request_no_name() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let result = email_service
            .send_support_request(
                "support@doubleword.ai",
                "alice@example.com",
                None,
                "Help with API keys",
                "I can't generate a new API key from the dashboard.",
            )
            .await;

        assert!(result.is_ok());
    }
}
