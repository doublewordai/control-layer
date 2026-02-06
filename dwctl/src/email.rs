//! Email service for sending password reset emails and notifications.

use fusillade::batch::BatchOutcome;
use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use std::path::Path;

use crate::{config::Config, errors::Error};

pub struct BatchCompletionInfo {
    pub batch_id: String,
    pub endpoint: String,
    pub outcome: BatchOutcome,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_requests: i64,
    pub completed_requests: i64,
    pub failed_requests: i64,
    pub dashboard_link: String,
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
        let body = self.create_password_reset_body(to_name, &reset_link);

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
        let body = self.create_batch_completion_body(to_name, info);
        self.send_email(to_email, to_name, &subject, &body).await
    }

    fn create_batch_completion_body(&self, to_name: Option<&str>, info: &BatchCompletionInfo) -> String {
        let greeting = if let Some(name) = to_name {
            format!("Hello {name},")
        } else {
            "Hello,".to_string()
        };

        let created_at = info.created_at.format("%Y-%m-%d %H:%M UTC").to_string();
        let finished_at = info
            .finished_at
            .map_or("—".to_string(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string());

        let (title, message) = match info.outcome {
            BatchOutcome::Completed => (
                "Batch completed",
                "Your batch has finished processing successfully."
            ),
            BatchOutcome::PartiallyCompleted => (
                "Batch completed with errors",
                "Your batch has finished processing, but some requests failed."
            ),
            BatchOutcome::Failed => (
                "Batch failed",
                "Your batch has finished processing, but all requests failed."
            ),
        };

        // Only show failed row if there were failures
        let failed_row = if info.failed_requests > 0 {
            format!("<tr><th>Failed</th><td>{}</td></tr>", info.failed_requests)
        } else {
            String::new()
        };

        format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>{title}</title>
    <style>
        body {{ font-family: Arial, sans-serif; line-height: 1.6; color: #333; }}
        .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
        table {{ border-collapse: collapse; width: 100%; margin: 16px 0; }}
        th, td {{ text-align: left; padding: 8px 12px; border-bottom: 1px solid #eee; }}
        th {{ color: #666; font-weight: normal; width: 160px; }}
        .footer {{ margin-top: 30px; font-size: 12px; color: #666; }}
    </style>
</head>
<body>
    <div class="container">
        <h2>{title}</h2>

        <p>{greeting}</p>

        <p>{message}</p>

        <table>
            <tr><th>Batch ID</th><td>{batch_id}</td></tr>
            <tr><th>Endpoint</th><td>{endpoint}</td></tr>
            <tr><th>Created</th><td>{created_at}</td></tr>
            <tr><th>Finished</th><td>{finished_at}</td></tr>
            <tr><th>Total requests</th><td>{total}</td></tr>
            <tr><th>Completed</th><td>{completed}</td></tr>
            {failed_row}
        </table>

        <p><a href="{dashboard_link}">View batch in dashboard</a></p>

        <div class="footer">
            <p>This is an automated message, please do not reply to this email.</p>
        </div>
    </div>
</body>
</html>"#,
            title = title,
            message = message,
            batch_id = info.batch_id,
            endpoint = info.endpoint,
            created_at = created_at,
            finished_at = finished_at,
            total = info.total_requests,
            completed = info.completed_requests,
            failed_row = failed_row,
            dashboard_link = info.dashboard_link,
        )
    }

    fn create_password_reset_body(&self, to_name: Option<&str>, reset_link: &str) -> String {
        let greeting = if let Some(name) = to_name {
            format!("Hello {name},")
        } else {
            "Hello,".to_string()
        };

        format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Password Reset Request</title>
    <style>
        body {{ font-family: Arial, sans-serif; line-height: 1.6; color: #333; }}
        .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
        .footer {{ margin-top: 30px; font-size: 12px; color: #666; }}
    </style>
</head>
<body>
    <div class="container">
        <h2>Password Reset Request</h2>

        <p>{greeting}</p>

        <p>We received a request to reset your password. If you didn't make this request, you can safely ignore this email.</p>

        <p>To reset your password, click the link below:</p>

        <p><a href="{reset_link}">Reset your password</a></p>

        <p>Or copy and paste this link into your browser:</p>
        <p>{reset_link}</p>

        <p>This link will expire in 30 minutes for security reasons.</p>

        <div class="footer">
            <p>If you're having trouble with the button above, copy and paste the URL into your web browser.</p>
            <p>This is an automated message, please do not reply to this email.</p>
        </div>
    </div>
</body>
</html>"#
        )
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

        let body = email_service.create_password_reset_body(Some("John Doe"), "https://example.com/reset?token=abc123");

        assert!(body.contains("Hello John Doe,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
        assert!(body.contains("Reset your password"));
    }

    #[tokio::test]
    async fn test_password_reset_email_body_no_name() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let body = email_service.create_password_reset_body(None, "https://example.com/reset?token=abc123");

        assert!(body.contains("Hello,"));
        assert!(body.contains("https://example.com/reset?token=abc123"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            outcome: BatchOutcome::Completed,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 100,
            failed_requests: 0,
            dashboard_link: "https://example.com/batches/abcd1234".to_string(),
        };

        let body = email_service.create_batch_completion_body(Some("Alice"), &info);

        assert!(body.contains("Hello Alice,"));
        assert!(body.contains("Batch completed"));
        assert!(body.contains("finished processing successfully"));
        assert!(body.contains("/v1/chat/completions"));
        assert!(body.contains("100"));
        assert!(!body.contains("Failed")); // No failed row when 0 failures
        assert!(body.contains("https://example.com/batches/abcd1234"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_partially_completed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            outcome: BatchOutcome::PartiallyCompleted,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 98,
            failed_requests: 2,
            dashboard_link: "https://example.com/batches/abcd1234".to_string(),
        };

        let body = email_service.create_batch_completion_body(Some("Alice"), &info);

        assert!(body.contains("Batch completed with errors"));
        assert!(body.contains("some requests failed"));
        assert!(body.contains("<th>Failed</th><td>2</td>"));
    }

    #[tokio::test]
    async fn test_batch_completion_email_body_failed() {
        let config = create_test_config();
        let email_service = EmailService::new(&config).unwrap();

        let info = BatchCompletionInfo {
            batch_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            outcome: BatchOutcome::Failed,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            total_requests: 100,
            completed_requests: 0,
            failed_requests: 100,
            dashboard_link: "https://example.com/batches/abcd1234".to_string(),
        };

        let body = email_service.create_batch_completion_body(Some("Alice"), &info);

        assert!(body.contains("Batch failed"));
        assert!(body.contains("all requests failed"));
        assert!(body.contains("<th>Failed</th><td>100</td>"));
    }
}
