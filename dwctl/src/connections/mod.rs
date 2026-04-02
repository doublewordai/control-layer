//! External data source connections and sync pipeline.
//!
//! This module provides:
//! - [`provider`]: Trait and implementations for source providers (S3, etc.)
//! - [`sync`]: Underway job definitions for the sync/ingest/activate pipeline

pub mod provider;
pub mod sync;
