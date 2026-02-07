//! Email service for sending password reset emails and notifications.

use fusillade::batch::BatchOutcome;
use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use std::path::Path;
use minijinja::{context, Environment};
use crate::{config::Config, errors::Error};

pub struct BatchCompletionInfo {
    pub batch_id: String,
    pub endpoint: String,
    pub model: String,
    pub outcome: BatchOutcome,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_requests: i64,
    pub completed_requests: i64,
    pub failed_requests: i64,
    pub dashboard_url: String,
    pub completion_window: String,
    pub filename: Option<String>,
    pub description: Option<String>,
}

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
        let email_config = &config.auth.native.email;

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
            reply_to: email_config.reply_to.clone()
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
        let body = self.render_password_reset_body(name, &reset_link)
            .map_err(|e| Error::Internal { operation: format!("render email template: {e}") })?;

        self.send_email(to_email, to_name, subject, &body).await
    }

    async fn send_email(&self, to_email: &str, to_name: Option<&str>, subject: &str, body: &str) -> Result<(), Error> {
        // Create from mailbox
        let from = format!("{} <{}>", self.from_name, self.from_email)
            .parse::<Mailbox>()
            .map_err(|e| Error::Internal {
                operation: format!("parse from email: {e}"),
            })?;

        // Create to mailbox
        let to = if let Some(name) = to_name {
            format!("{name} <{to_email}>")
        } else {
            to_email.to_string()
        }
        .parse::<Mailbox>()
        .map_err(|e| Error::Internal {
            operation: format!("parse to email: {e}"),
        })?;

        let mut builder = Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .header(ContentType::TEXT_HTML);

        if let Some(ref reply_to_email) = self.reply_to {
            let reply_to = format!("{} <{reply_to_email}>", self.from_name)
                .parse::<Mailbox>()
                .map_err(|e| Error::Internal {
                    operation: format!("parse reply-to email: {e}"),
                })?;
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
        info: &BatchCompletionInfo,
    ) -> Result<(), Error> {
        let status_text = match info.outcome {
            BatchOutcome::Completed => "completed",
            BatchOutcome::PartiallyCompleted => "completed with errors",
            BatchOutcome::Failed => "failed",
        };
        let subject = format!("Batch {} — {}", &info.batch_id[..8.min(info.batch_id.len())], status_text);
        let name = to_name.unwrap_or("User");
        let body = self.render_batch_completion_body_non_first(name.to_string(), info)
            .map_err(|e| Error::Internal { operation: format!("render email template: {e}") })?;
        self.send_email(to_email, to_name, &subject, &body).await
    }

    pub fn render_batch_completion_body_non_first(&self, to_name: String, info: &BatchCompletionInfo) -> Result<String, minijinja::Error> {
        let mut env = Environment::new();
        env.add_template("email", include_str!("../../email_templates/batch_complete.html"))?;

        let (outcome_label, outcome_icon, header_color, outcome_message) = match info.outcome {
            BatchOutcome::Completed => ("Completed", "✓", "#16a34a", "Your batch has finished processing successfully."),
            BatchOutcome::PartiallyCompleted => ("Completed with some failures", "⚠", "#d97706", "Your batch has finished processing, but some requests failed."),
            BatchOutcome::Failed => ("Failed", "✗", "#dc2626", "There was a problem processing your batch."),
        };

        let duration = info.finished_at.map(|finished| {
            let dur = finished - info.created_at;
            let total_secs = dur.num_seconds();
            if total_secs < 60 {
                format!("{total_secs}s")
            } else if total_secs < 3600 {
                format!("{}m {}s", total_secs / 60, total_secs % 60)
            } else {
                format!("{}h {}m", total_secs / 3600, (total_secs % 3600) / 60)
            }
        }).unwrap_or_default();

        let base = info.dashboard_url.trim_end_matches('/');
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

        let body = email_service.render_password_reset_body("John Doe", "https://example.com/reset?token=abc123").unwrap();

        assert!(body.contains("Hello John Doe,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
        assert!(body.contains("Reset your password"));
    }

    #[tokio::test]
    async fn test_password_reset_email_body_no_name() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let body = email_service.render_password_reset_body("User", "https://example.com/reset?token=abc123").unwrap();

        assert!(body.contains("Hello User,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "gpt-4o".to_string(),
            outcome: BatchOutcome::Completed,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 100,
            failed_requests: 0,
            dashboard_url: "https://example.com".to_string(),
            completion_window: "24h".to_string(),
            filename: Some("input.jsonl".to_string()),
            description: Some("Weekly report generation".to_string()),
        };

        let body = email_service.render_batch_completion_body_non_first("Alice".into(), &info).unwrap();

        assert!(body.contains("Hi Alice,"));
        assert!(body.contains("Completed"));
        assert!(body.contains("finished processing successfully"));
        assert!(body.contains("/v1/chat/completions"));
        assert!(body.contains("gpt-4o"));
        assert!(body.contains("100"));
        assert!(body.contains("https://example.com/batches/abcd1234-5678-90ab-cdef-1234567890ab"));
        assert!(body.contains("https://example.com/profile"));
        assert!(body.contains("24h"));
        assert!(body.contains("input.jsonl"));
        assert!(body.contains("Weekly report generation"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_partially_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "gpt-4o".to_string(),
            outcome: BatchOutcome::PartiallyCompleted,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 98,
            failed_requests: 2,
            dashboard_url: "https://example.com".to_string(),
            completion_window: "24h".to_string(),
            filename: Some("input.jsonl".to_string()),
            description: None,
        };

        let body = email_service.render_batch_completion_body_non_first("Alice".into(), &info).unwrap();

        assert!(body.contains("Completed with some failures"));
        assert!(body.contains("some requests failed"));
        assert!(body.contains(">2<"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_failed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "gpt-4o".to_string(),
            outcome: BatchOutcome::Failed,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 0,
            failed_requests: 100,
            dashboard_url: "https://example.com".to_string(),
            completion_window: "24h".to_string(),
            filename: None,
            description: None,
        };

        let body = email_service.render_batch_completion_body_non_first("Alice".into(), &info).unwrap();

        assert!(body.contains("Failed"));
        assert!(body.contains("problem processing your batch"));
        assert!(body.contains(">100<"));
    }
}
