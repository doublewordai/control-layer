//! Cached-input pricing subsystem: an Anthropic-style prompt cache, owned by dwctl.
//!
//! **dwctl-owned.** Caching is implemented entirely here, as tower layers wrapping the
//! (cache-agnostic) onwards router: a request layer forks `classify()` in parallel
//! with the upstream call and strips markers; a response layer injects the
//! [`CacheStats`] into the usage and commits the [`PendingWrite`] locally on success.
//! onwards knows nothing about caching.
//!
//! Modules: the classify engine — [`CacheIndex`]/`PostgresIndex`, the tokenizer-svc
//! client, the [`PrincipalResolver`], the [`ModelConfigResolver`], and the `parse`r;
//! the neutral [`CacheStats`]/[`PendingWrite`]; (later) `inject` + the `layer`.

pub mod classifier;
pub mod index;
pub mod inject;
pub mod layer;
pub mod metrics;
pub mod model_config;
pub mod parse;
pub mod postgres;
pub mod principal;
pub mod sse;
pub mod stats;
pub mod tokenizer;

pub use classifier::{Classifier, ClassifyOutcome, ClassifyRequest};
pub use index::{CacheEntry, CacheError, CacheIndex, CacheMatch, CacheResult, IndexScope, PrefixHash, TtlTier};
pub use inject::{CommitGate, inject_cache_stats_into_response, strip_cache_control};
pub use layer::{CacheLayerState, cache_middleware};
pub use model_config::{ModelCacheConfig, ModelConfigResolver};
pub use parse::{Block, Breakpoint, ParseError, ParsedPrompt, parse_chat_completions};
pub use postgres::PostgresIndex;
pub use principal::PrincipalResolver;
pub use stats::{CacheStats, PendingWrite};
pub use tokenizer::{ModelInfo, TokenizeResponse, TokenizerClient, TokenizerError, TokenizerResult};
