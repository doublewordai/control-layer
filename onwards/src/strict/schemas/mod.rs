//! OpenAI API request and response schemas for strict validation
//!
//! These schemas are used by the strict router to validate requests before forwarding.
//! They're designed to accept valid OpenAI API requests while providing helpful error
//! messages for malformed requests.

pub mod chat_completions;
pub mod completions;
pub mod embeddings;
pub mod responses;
pub mod utils;
