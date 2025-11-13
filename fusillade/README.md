# Fusillade

A batching system for HTTP requests with retry logic and per-model concurrency
control. Requests in their various states are stored persistently in a postgres
database (see ./migrations/ for the schema), which is connected to with the
[sqlx](https://github.com/launchbadge/sqlx) library.

Lists of requests can be dispatched as 'files', from which 'batches' can be
spawned. The behaviour is inspired by the OpenAI [Batch API](https://platform.openai.com/docs/guides/batch).

## Usage

Create a file with a list of request 'templates'. Create a batch from that file
to execute all of its requests. Then track progress of each request in the
batch as they're executed by the daemon.

- **Files** group related request templates
- **Request templates** define HTTP requests (endpoint, method, body, API key)
- **Batches** snapshot all templates in a file and start executing them.
Multiple batches can be triggered from a single file.
- **Requests** are created from templates (one per batch) and progress through
states as the daemon processes them

### Basic Example

```rust
use fusillade::{PostgresRequestManager, RequestTemplateInput};
use std::sync::Arc;
use sqlx::PgPool;

// Setup
let pool = PgPool::connect("postgresql://localhost/fusillade").await?;
let manager = Arc::new(PostgresRequestManager::new(pool));

// Start the daemon
let daemon_handle = manager.clone().run()?;

// Create a file with request templates
let file_id = manager.create_file(
    "completions".to_string(),
    Some("GPT-4 completions batch".to_string()),
    vec![
        RequestTemplateInput {
            endpoint: "https://api.openai.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: env::var("OPENAI_API_KEY")?,
        },
    ],
).await?;

// Launch a batch from that file
let batch_id = manager.create_batch(file_id).await?;

// Check the status of the batch
let status = manager.get_batch_status(batch_id).await?;
println!("Completed: {}/{}", status.completed_requests, status.total_requests);
```

### Concurrency Control

Fusillade allows setting per-model concurrency limits:

```rust
use std::collections::HashMap;
use fusillade::DaemonConfig;

let mut model_limits = HashMap::new();
model_limits.insert("gpt-4".to_string(), 5);    // Max 5 concurrent GPT-4 requests
model_limits.insert("gpt-3.5-turbo".to_string(), 20);

let config = DaemonConfig {
    model_concurrency_limits: model_limits,
    max_retries: 3,
    backoff_ms: 1000,
    ..Default::default()
};

let manager = Arc::new(PostgresRequestManager::new(pool).with_config(config));
```

### Tracking Requests

To get the status of all requests in a batch:

```rust
// Get all requests for a batch
let requests = manager.get_batch_requests(batch_id).await?;

for req in requests {
    match req {
        AnyRequest::Completed(r) => {
            println!("Request {} completed: {}", r.data.id, r.state.response_body);
        }
        AnyRequest::Failed(r) => {
            println!("Request {} failed: {}", r.data.id, r.state.error);
        }
        _ => {}
    }
}
```

## Database Setup

Run migrations before first use, by importing the migrator and executing it against your database pool:

```rust
fusillade::migrator().run(&pool).await?;
```
