//! Email service for sending password reset emails and notifications.

use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use std::path::Path;

use crate::{config::Config, errors::Error};

pub struct EmailService {
    transport: EmailTransport,
    from_email: String,
    from_name: String,
    base_url: String,
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
            base_url: email_config.password_reset.base_url.clone(),
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

        // Build message
        let message = Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .header(ContentType::TEXT_HTML)
            .body(body.to_string())
            .map_err(|e| Error::Internal {
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
    use crate::test_utils::create_test_config;

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
}
