//! Email service for sending password reset emails and notifications

use crate::notifications::{BatchNotificationInfo, BatchOutcome};
use crate::{config::Config, errors::Error};
use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use minijinja::{Environment, context};
use std::path::Path;

pub struct EmailService {
    transport: EmailTransport,
    from_email: String,
    from_name: String,
    base_url: String,
    reply_to: Option<String>,
}

enum EmailTransport {
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    File(AsyncFileTransport<Tokio1Executor>),
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
        };

        Ok(Self {
            transport,
            from_email: email_config.from_email.clone(),
            from_name: email_config.from_name.clone(),
            base_url: config.dashboard_url.clone(),
            reply_to: email_config.reply_to.clone(),
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

        self.send_email(to_email, to_name, subject, &body).await
    }

    async fn send_email(&self, to_email: &str, to_name: Option<&str>, subject: &str, body: &str) -> Result<(), Error> {
        // Create from mailbox
        let from_address = self.from_email.parse().map_err(|e| Error::Internal {
            operation: format!("Failed to parse from email: {e}"),
        })?;
        let from = Mailbox::new(Some(self.from_name.clone()), from_address);

        // Create to mailbox
        let to_address = to_email.parse().map_err(|e| Error::Internal {
            operation: format!("Failed to parse to email: {e}"),
        })?;
        let to = Mailbox::new(to_name.map(|n| n.to_string()), to_address);

        let mut builder = Message::builder().from(from).to(to).subject(subject).header(ContentType::TEXT_HTML);

        if let Some(ref reply_to_email) = self.reply_to {
            let reply_to_address = reply_to_email.parse().map_err(|e| Error::Internal {
                operation: format!("Failed to parse reply-to email: {e}"),
            })?;
            let reply_to = Mailbox::new(Some(self.from_name.clone()), reply_to_address);
            builder = builder.reply_to(reply_to);
        }

        let message = builder.body(body.to_string()).map_err(|e| Error::Internal {
            operation: format!("build email message: {e}"),
        })?;

        // Send based on transport type
        match &self.transport {
            EmailTransport::Smtp(smtp) => {
                smtp.send(message).await.map_err(|e| Error::Internal {
                    operation: format!("send SMTP email: {e}"),
                })?;
            }
            EmailTransport::File(file) => {
                file.send(message).await.map_err(|e| Error::Internal {
                    operation: format!("send file email: {e}"),
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
        self.send_email(to_email, to_name, &subject, &body).await
    }

    pub fn render_batch_completion_body(
        &self,
        to_name: String,
        info: &BatchNotificationInfo,
        first_batch: bool,
    ) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        if first_batch {
            env.add_template("email", include_str!("../../email_templates/first_batch.html"))?;
        } else {
            env.add_template("email", include_str!("../../email_templates/batch_complete.html"))?;
        }

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
            completion_window => &info.completion_window,
            filename => info.filename.as_deref().unwrap_or(""),
            description => info.description.as_deref().unwrap_or(""),
            from_name => &self.from_name,
            reply_to => self.reply_to.as_deref().unwrap_or(&self.from_email),
        })
    }

    fn render_password_reset_body(&self, to_name: &str, reset_link: &str) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", include_str!("../../email_templates/password_reset.html"))?;

        env.get_template("email")?.render(context! {
            to_name,
            reset_link,
        })
    }
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
        assert!(body.contains("Your results are ready!"));
        assert!(body.contains("http://localhost:3001/batches/abcd1234-5678-90ab-cdef-1234567890ab"));
        assert!(body.contains("Batch Complete"));
        assert!(body.contains("Run another batch"));
        assert!(body.contains("Autobatcher"));
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
            let result = email_service.send_email(email, name, "Test Subject", "<p>Hello</p>").await;
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
}
