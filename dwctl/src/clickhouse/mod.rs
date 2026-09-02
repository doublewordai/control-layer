//! ClickHouse Cloud: the shared warehouse connection and a generic best-effort sink.
//!
//! The warehouse (`clay`) is fed mostly by the Neon → ClickHouse CDC pipe. A few
//! high-volume, derived, analytics-only records are written directly from dwctl instead,
//! because they are variable-length, never read back by dwctl, and would only add WAL
//! and storage to the billing database on their way through. This module is the one
//! connection those writers share ([`ClickHouseClient`], built from the top-level
//! `clickhouse` config section) and the sink they hang off ([`sink::ClickHouseSink`]).
//!
//! Best effort by design. Nothing on the request path waits on it. A full queue drops
//! the newest rows, a failed insert is retried once and then dropped, and every drop is
//! counted (`dwctl_clickhouse_dropped_rows_total`). There is no spill and no replay: a
//! warehouse outage loses that window's records, which is the agreed trade for these
//! records (2026-09-02).
//!
//! Rows land in batches: one `INSERT ... FORMAT JSONEachRow` per flush, gzip-compressed.
//! ClickHouse's cost is per insert statement (each creates a part), not per row, so the
//! sink flushes on a timer of seconds rather than per record.

use std::fmt;
use std::io::Write as _;
use std::time::Duration;

use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use url::Url;

pub mod sink;
pub use sink::{ClickHouseSink, SinkHandle, SinkOptions};

/// Header ClickHouse reads the user from on the HTTP interface.
const USER_HEADER: &str = "X-ClickHouse-User";
/// Header ClickHouse reads the password from on the HTTP interface.
const KEY_HEADER: &str = "X-ClickHouse-Key";
/// How much of an error body to keep in the error (the rest is noise).
const ERROR_BODY_LIMIT: usize = 512;

fn default_database() -> String {
    "clay".to_string()
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(30)
}

/// The top-level `clickhouse` config section: connection and credential only.
///
/// Table names, flush cadence and queue sizing belong to the feature that writes (a
/// second writer wants its own), so they live on that feature's section. Absent section
/// = no feature may write to ClickHouse; a feature that is enabled without this section
/// fails at startup rather than silently publishing nothing (see `Config::validate`).
#[derive(Clone, Serialize, Deserialize)]
pub struct ClickhouseConfig {
    /// HTTPS endpoint of the ClickHouse Cloud service, e.g.
    /// `https://<service>.eu-west-2.aws.clickhouse.cloud:8443`.
    pub endpoint_url: String,
    /// Database every table name is qualified with. Defaults to `clay`.
    #[serde(default = "default_database")]
    pub database: String,
    /// User to authenticate as. Grant it INSERT on exactly the tables it writes.
    pub user: String,
    /// Password. `DWCTL_CLICKHOUSE__PASSWORD` in deployments.
    pub password: String,
    /// Per-insert HTTP timeout. ClickHouse Cloud services can idle and take seconds to
    /// wake, so keep this generous; the sink is off the request path anyway.
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    pub request_timeout: Duration,
}

impl fmt::Debug for ClickhouseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickhouseConfig")
            .field("endpoint_url", &redact_url(&self.endpoint_url))
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl ClickhouseConfig {
    /// Startup validation: a well-formed http(s) endpoint, a database and user that are
    /// not blank, and a non-empty password. The password check is what turns "secret not
    /// mounted" into a startup error instead of a sink that 401s forever.
    pub fn validate(&self) -> Result<(), String> {
        let url = Url::parse(self.endpoint_url.trim()).map_err(|e| format!("clickhouse.endpoint_url is not a valid URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("clickhouse.endpoint_url must be http(s), got {:?}", url.scheme()));
        }
        if !is_identifier(&self.database) {
            return Err(format!("clickhouse.database {:?} is not a plain identifier", self.database));
        }
        if self.user.trim().is_empty() {
            return Err("clickhouse.user is empty".to_string());
        }
        if self.password.is_empty() {
            return Err("clickhouse.password is empty (set DWCTL_CLICKHOUSE__PASSWORD)".to_string());
        }
        if self.request_timeout.is_zero() {
            return Err("clickhouse.request_timeout must be > 0".to_string());
        }
        if url.password().is_some() || !url.username().is_empty() {
            return Err("clickhouse.endpoint_url must not embed credentials; use clickhouse.user and clickhouse.password".to_string());
        }
        Ok(())
    }
}

/// A URL safe to put in logs: any userinfo replaced, everything else intact. Falls back
/// to a placeholder rather than the raw string when the URL does not parse, since an
/// unparseable value can still hold a secret.
fn redact_url(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(mut url) => {
            if url.password().is_some() || !url.username().is_empty() {
                let _ = url.set_username("<redacted>");
                let _ = url.set_password(None);
            }
            url.to_string()
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}

/// A database or table name we are willing to splice into an `INSERT` statement. The
/// names come from config, not from users, but the statement is built by string
/// formatting, so keep them to plain identifiers.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Debug, thiserror::Error)]
pub enum ClickHouseError {
    #[error("invalid clickhouse config: {0}")]
    Config(String),
    #[error("clickhouse request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("clickhouse returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("failed to compress insert body: {0}")]
    Compress(#[from] std::io::Error),
}

/// A thin HTTP client for the ClickHouse Cloud HTTP interface.
///
/// One method: insert a `JSONEachRow` body into a table. No reads, no DDL — the writer
/// role is INSERT-only and this client is shaped to match.
#[derive(Clone)]
pub struct ClickHouseClient {
    http: reqwest::Client,
    endpoint: Url,
    database: String,
    user: String,
    password: String,
}

impl fmt::Debug for ClickHouseClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseClient")
            .field("endpoint", &redact_url(self.endpoint.as_str()))
            .field("database", &self.database)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl ClickHouseClient {
    pub fn from_config(config: &ClickhouseConfig) -> Result<Self, ClickHouseError> {
        config.validate().map_err(ClickHouseError::Config)?;
        let endpoint = Url::parse(config.endpoint_url.trim()).map_err(|e| ClickHouseError::Config(e.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(ClickHouseError::Transport)?;
        Ok(Self {
            http,
            endpoint,
            database: config.database.clone(),
            user: config.user.clone(),
            password: config.password.clone(),
        })
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    /// The URL an insert into `table` posts to. The statement rides in the `query`
    /// parameter and the body is the data, which is the HTTP interface's insert form.
    ///
    /// `date_time_input_format=best_effort` is load-bearing: our timestamps are RFC3339
    /// with a trailing `Z` (chrono's `DateTime<Utc>`), which the default parser rejects.
    fn insert_url(&self, table: &str) -> Url {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("query", &format!("INSERT INTO {}.{} FORMAT JSONEachRow", self.database, table))
            .append_pair("date_time_input_format", "best_effort");
        url
    }

    /// Insert newline-delimited JSON rows into `table`. The body is gzip-compressed on
    /// the way out (ClickHouse decompresses on `Content-Encoding: gzip`). Any non-2xx is
    /// an error carrying the first bytes of ClickHouse's message, which names the column
    /// or row that failed.
    pub async fn insert_json_each_row(&self, table: &str, json_lines: &[u8]) -> Result<(), ClickHouseError> {
        if !is_identifier(table) {
            return Err(ClickHouseError::Config(format!("table {table:?} is not a plain identifier")));
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::with_capacity(json_lines.len() / 4 + 64), flate2::Compression::fast());
        gz.write_all(json_lines)?;
        let body = gz.finish()?;

        let response = self
            .http
            .post(self.insert_url(table))
            .header(USER_HEADER, &self.user)
            .header(KEY_HEADER, &self.password)
            .header(CONTENT_ENCODING, "gzip")
            .header(CONTENT_TYPE, "application/x-ndjson")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let mut body = response.text().await.unwrap_or_default();
        if body.len() > ERROR_BODY_LIMIT {
            let mut cut = ERROR_BODY_LIMIT;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push('…');
        }
        Err(ClickHouseError::Status {
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ClickhouseConfig {
        ClickhouseConfig {
            endpoint_url: "https://example.clickhouse.cloud:8443".to_string(),
            database: default_database(),
            user: "dwctl".to_string(),
            password: "pw".to_string(),
            request_timeout: default_request_timeout(),
        }
    }

    #[test]
    fn config_validates_endpoint_identifiers_and_password() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.endpoint_url = "not a url".into();
        assert!(c.validate().unwrap_err().contains("endpoint_url"));
        let mut c = config();
        c.endpoint_url = "ftp://x".into();
        assert!(c.validate().unwrap_err().contains("http(s)"));
        let mut c = config();
        c.database = "clay; DROP".into();
        assert!(c.validate().unwrap_err().contains("identifier"));
        let mut c = config();
        c.password = String::new();
        assert!(c.validate().unwrap_err().contains("password"));
    }

    #[test]
    fn debug_output_redacts_the_password() {
        let s = format!("{:?}", config());
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("pw\""));
    }

    #[test]
    fn debug_output_redacts_credentials_embedded_in_the_url() {
        let mut c = config();
        c.endpoint_url = "https://alice:s3cret@example.clickhouse.cloud:8443".into();
        let s = format!("{:?}", c);
        assert!(!s.contains("s3cret") && !s.contains("alice"), "{s}");
        assert!(c.validate().unwrap_err().contains("embed credentials"));
        assert_eq!(redact_url("not a url"), "<unparseable url>");
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let mut c = config();
        c.request_timeout = Duration::ZERO;
        assert!(c.validate().unwrap_err().contains("request_timeout"));
    }

    #[test]
    fn identifiers_are_plain() {
        assert!(is_identifier("prompt_chains"));
        assert!(is_identifier("_t1"));
        assert!(!is_identifier("1abc"));
        assert!(!is_identifier("a-b"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("t;x"));
    }

    #[test]
    fn insert_url_carries_the_statement_and_the_datetime_setting() {
        let c = ClickHouseClient::from_config(&config()).unwrap();
        let url = c.insert_url("prompt_chains");
        let pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        assert!(pairs.contains(&("query".into(), "INSERT INTO clay.prompt_chains FORMAT JSONEachRow".into())));
        assert!(pairs.contains(&("date_time_input_format".into(), "best_effort".into())));
    }

    #[tokio::test]
    async fn insert_rejects_a_bad_table_name_before_any_request() {
        let c = ClickHouseClient::from_config(&config()).unwrap();
        let err = c.insert_json_each_row("bad name", b"{}\n").await.unwrap_err();
        assert!(matches!(err, ClickHouseError::Config(_)));
    }
}
