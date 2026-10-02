//! Serving-scoped retry of tokenizer-svc overloads (HTTP 503).
//!
//! Only the *serving* classify path retries tokenizer-svc, and only for as long as the owning
//! request lifecycle keeps the task alive (inference time plus the existing classify join grace).
//! There is deliberately **no attempt cap and no overall retry deadline** in the loop below:
//! recovery is bounded by cancellation (task abort / future drop), a non-retryable error, or
//! success. Non-serving callers (recompute, replay, prefix-chain, admin, tests using
//! [`TokenizerClient::new`](super::tokenizer::TokenizerClient::new)) make exactly one attempt.
//!
//! Retry semantics:
//! - Retry only HTTP `503` (any body: a structured overload code or a proxy error page) for
//!   `tokenize` / `render` / `models`. `400`/`422`/other statuses, malformed `2xx`, and transport
//!   errors/timeouts are returned to the caller unchanged and are **not** retried.
//! - Each retry sleeps equal-jitter exponential backoff, then acquires the shared budget
//!   (a concurrency permit plus a rate token) before its HTTP attempt. The permit is held only
//!   for the duration of the attempt — never across backoff.
//! - **`Retry-After` is not honored** in this change; backoff is purely the local equal-jitter
//!   schedule. (The per-HTTP-attempt timeout remains 5s and applies to every attempt.)
//! - The budget is shared across every client clone for a destination (one `Arc` per process),
//!   so it bounds total retry load: a token-bucket rate limit (`retries_per_second` / `retry_burst`)
//!   and a retry-attempt concurrency limit (`max_concurrent_retries`). First attempts never
//!   consume the budget.
//!
//! The token bucket is refilled from [`tokio::time::Instant`] elapsed time (rather than by
//! arrivals), so a waiter wakes and proceeds once enough virtual/real time has passed even with
//! no other traffic. All waits are cancellation-safe: dropping the retrying future releases any
//! held permit and stops future attempts.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

use super::metrics;
use super::tokenizer::{TokenizerError, TokenizerResult};

/// Validated retry settings. Built once from `cache.tokenizer_retry` config in `lib.rs`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenizerRetryPolicy {
    /// First nominal backoff (equal-jitter: actual delay ∈ [nominal/2, nominal]).
    pub initial_backoff: Duration,
    /// Cap on the nominal backoff (doubling saturates here).
    pub max_backoff: Duration,
    /// Shared token-bucket refill rate: retry starts per second, per process/destination.
    pub retries_per_second: u32,
    /// Shared token-bucket capacity (burst).
    pub retry_burst: u32,
    /// Max simultaneous retry HTTP attempts per process/destination.
    pub max_concurrent_retries: u32,
}

impl Default for TokenizerRetryPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            retries_per_second: 10,
            retry_burst: 10,
            max_concurrent_retries: 8,
        }
    }
}

/// Time-refilled token bucket. `tokens` is fractional so partial refills are not lost; capacity
/// and rate are clamped to at least 1 so a misconfigured zero can never deadlock the loop.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// Shared retry budget: time-refilled token bucket + retry-attempt concurrency limit.
/// Construct ONCE per process/destination and share via `Arc` across every client clone.
pub struct TokenizerRetryBudget {
    policy: TokenizerRetryPolicy,
    semaphore: Semaphore,
    bucket: Mutex<TokenBucket>,
    /// In-flight retry attempts. A mutex (not an atomic) so the count and the gauge `set` happen
    /// together — two racing transitions can't leave the gauge on a stale value.
    inflight: Mutex<usize>,
}

impl TokenizerRetryBudget {
    pub fn new(policy: TokenizerRetryPolicy) -> Arc<Self> {
        let capacity = bucket_capacity(&policy);
        Arc::new(Self {
            semaphore: Semaphore::new(policy.max_concurrent_retries.max(1) as usize),
            bucket: Mutex::new(TokenBucket {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
            inflight: Mutex::new(0),
            policy,
        })
    }

    pub fn policy(&self) -> &TokenizerRetryPolicy {
        &self.policy
    }

    /// Wait for a concurrency permit *and* a rate token, then return a guard that holds the
    /// permit for the duration of one retry attempt.
    ///
    /// The permit is released before sleeping for a rate token, so one caller waiting on the rate
    /// limit never blocks a peer that already has a token from taking a concurrency slot. Waiting
    /// is unbounded and cancellation-safe: a dropped future releases everything it held.
    pub(crate) async fn acquire(&self) -> RetryAttemptGuard<'_> {
        loop {
            let permit = self.semaphore.acquire().await.expect("retry semaphore is never closed");
            if self.try_take_token() {
                self.adjust_inflight(true);
                return RetryAttemptGuard {
                    budget: self,
                    _permit: permit,
                };
            }
            // No token: do not hoard the concurrency slot while waiting for the rate limiter.
            drop(permit);
            let wait = self.duration_until_token();
            tokio::time::sleep(wait).await;
        }
    }

    fn adjust_inflight(&self, up: bool) {
        let mut n = self.inflight.lock().expect("retry inflight mutex is not poisoned");
        *n = if up { *n + 1 } else { n.saturating_sub(1) };
        metrics::set_tokenizer_retry_inflight(*n);
    }

    /// Refill from elapsed wall (virtual) time and take one token if available.
    fn try_take_token(&self) -> bool {
        let mut bucket = self.bucket.lock().expect("retry bucket mutex is not poisoned");
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.last_refill);
        let capacity = bucket_capacity(&self.policy);
        bucket.tokens = (bucket.tokens + elapsed.as_secs_f64() * bucket_rate(&self.policy)).min(capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until the next token, given the current fill. Recomputed from the bucket so a
    /// refill that happened while a waiter was queued is accounted for.
    fn duration_until_token(&self) -> Duration {
        let bucket = self.bucket.lock().expect("retry bucket mutex is not poisoned");
        let deficit = (1.0 - bucket.tokens).max(0.0);
        Duration::from_secs_f64(deficit / bucket_rate(&self.policy))
    }
}

/// RAII guard for one in-flight retry attempt: releases the concurrency permit and decrements the
/// in-flight gauge on every path, including cancellation (future drop).
pub(crate) struct RetryAttemptGuard<'a> {
    budget: &'a TokenizerRetryBudget,
    _permit: SemaphorePermit<'a>,
}

impl Drop for RetryAttemptGuard<'_> {
    fn drop(&mut self) {
        self.budget.adjust_inflight(false);
    }
}

fn bucket_capacity(policy: &TokenizerRetryPolicy) -> f64 {
    policy.retry_burst.max(1) as f64
}

fn bucket_rate(policy: &TokenizerRetryPolicy) -> f64 {
    policy.retries_per_second.max(1) as f64
}

/// Nominal (pre-jitter) exponential backoff for retry number `n` (1-based): `initial * 2^(n-1)`,
/// capped at `max_backoff`. Saturating throughout so `n == u32::MAX` neither overflows nor panics.
fn backoff_nominal(policy: &TokenizerRetryPolicy, n: u32) -> Duration {
    let max = policy.max_backoff;
    let initial = policy.initial_backoff.min(max);
    if initial.is_zero() || n <= 1 {
        return initial;
    }
    // `checked_shl` yields None once the shift exceeds u128, in which case the result is already
    // far past any sane cap.
    let factor = 1u128.checked_shl(n - 1).unwrap_or(u128::MAX);
    let nanos = initial.as_nanos().saturating_mul(factor);
    match u64::try_from(nanos) {
        Ok(n) if nanos < max.as_nanos() => Duration::from_nanos(n),
        // At/above the cap, or beyond what a `Duration::from_nanos` can hold: the cap.
        _ => max,
    }
}

/// Equal jitter: a delay uniform in `[nominal/2, nominal]`, so retries spread out without ever
/// sleeping longer than the nominal (capped) backoff.
fn equal_jitter(nominal: Duration) -> Duration {
    if nominal.is_zero() {
        return Duration::ZERO;
    }
    let half = nominal / 2;
    let span = nominal - half;
    let fraction = rand::random_range(0.0f64..1.0);
    half + span.mul_f64(fraction)
}

/// Drive one logical operation with 503 retries.
///
/// `attempt(is_retry)` performs exactly one HTTP attempt and returns its result. When `budget` is
/// `Some`, a `503` is retried indefinitely (backoff, then budget, then attempt) until success, a
/// non-retryable result, or cancellation. When `budget` is `None` the operation runs exactly once.
/// No attempt cap and no deadline: cancellation of the returned future is the only stop for a
/// sustained outage.
pub(crate) async fn run_with_retries<T, F, Fut>(
    budget: Option<&TokenizerRetryBudget>,
    op: &'static str,
    mut attempt: F,
) -> TokenizerResult<T>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = TokenizerResult<T>>,
{
    let mut retries: u32 = 0;
    loop {
        let is_retry = retries > 0;

        // Retry attempts wait out backoff first, then check out a concurrency permit + rate
        // token. The guard is held across the attempt below and dropped (releasing the permit)
        // as soon as it finishes — or if this future is cancelled.
        let guard = match budget {
            Some(budget) if is_retry => {
                let nominal = backoff_nominal(budget.policy(), retries);
                if !nominal.is_zero() {
                    tokio::time::sleep(equal_jitter(nominal)).await;
                }
                let wait_start = Instant::now();
                let guard = budget.acquire().await;
                metrics::record_tokenizer_retry_budget_wait(op, wait_start.elapsed().as_secs_f64());
                Some(guard)
            }
            _ => None,
        };

        let result = attempt(is_retry).await;
        drop(guard);

        let attempt_label = if is_retry { "retry" } else { "first" };
        match result {
            Ok(value) => {
                metrics::record_tokenizer_attempt(op, attempt_label, "ok");
                if is_retry {
                    metrics::record_tokenizer_retry_recovered(op);
                }
                return Ok(value);
            }
            Err(err) => {
                metrics::record_tokenizer_attempt(op, attempt_label, attempt_result_label(&err));
                let retryable = matches!(err, TokenizerError::Status { status: 503, .. });
                if budget.is_none() || !retryable {
                    return Err(err);
                }
                retries = retries.saturating_add(1);
            }
        }
    }
}

fn attempt_result_label(err: &TokenizerError) -> &'static str {
    match err {
        TokenizerError::Status { status: 503, .. } => "overloaded_503",
        TokenizerError::Status { .. } => "http_error",
        TokenizerError::Http(_) => "transport_error",
        TokenizerError::Unmapped(_) | TokenizerError::RenderUnsupported(..) => "http_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    fn fast_policy() -> TokenizerRetryPolicy {
        TokenizerRetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            retries_per_second: 1_000,
            retry_burst: 1_000,
            max_concurrent_retries: 8,
        }
    }

    fn overloaded() -> TokenizerError {
        TokenizerError::Status {
            status: 503,
            body: "overloaded".to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn several_503s_then_success() {
        let budget = TokenizerRetryBudget::new(fast_policy());
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result = run_with_retries(Some(&budget), "tokenize", move |_is_retry| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 4 { Err(overloaded()) } else { Ok(n) }
            }
        })
        .await
        .expect("operation recovers");

        assert_eq!(result, 4);
        assert!(attempts.load(Ordering::SeqCst) > 2, "expected multiple retries");
    }

    #[tokio::test(start_paused = true)]
    async fn survives_more_than_five_seconds_of_virtual_time() {
        // Retrying across >5s of paused time proves the loop has no hidden overall deadline.
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(2),
            retries_per_second: 1_000,
            retry_burst: 1_000,
            max_concurrent_retries: 8,
        });
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let start = Instant::now();
        let result = run_with_retries(Some(&budget), "tokenize", move |_is_retry| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 5 { Err(overloaded()) } else { Ok(n) }
            }
        })
        .await
        .expect("operation recovers");

        assert_eq!(result, 6);
        assert!(
            start.elapsed() > Duration::from_secs(5),
            "virtual elapsed {:?} should exceed 5s",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn token_bucket_refills_without_new_arrivals() {
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            retries_per_second: 1,
            retry_burst: 1,
            max_concurrent_retries: 8,
        });

        // Starts full: the first token is immediate.
        let first = budget.acquire().await;
        drop(first);

        // Depleted, and no further arrivals: the waiter must sleep until the bucket refills.
        let start = Instant::now();
        let second = budget.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "waited only {:?} for refill",
            start.elapsed()
        );
        drop(second);
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_one_budget() {
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            retries_per_second: 1,
            retry_burst: 1,
            max_concurrent_retries: 8,
        });
        let clone = budget.clone();

        let first = budget.acquire().await;
        let second = tokio::time::timeout(Duration::from_millis(100), clone.acquire()).await;
        assert!(second.is_err(), "clone must share the depleted budget");
        drop(first);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_limit_applies_to_retry_attempts() {
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            retries_per_second: 10_000,
            retry_burst: 10_000,
            max_concurrent_retries: 2,
        });
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let budget = budget.clone();
            let inflight = inflight.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                run_with_retries(Some(&budget), "tokenize", move |is_retry| {
                    let inflight = inflight.clone();
                    let peak = peak.clone();
                    async move {
                        if !is_retry {
                            return Err(overloaded());
                        }
                        let current = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        Ok(1u32)
                    }
                })
                .await
            }));
        }
        for handle in handles {
            handle.await.expect("task does not panic").expect("operation recovers");
        }

        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "observed {} concurrent retry attempts",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_releases_permit() {
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            retries_per_second: 10_000,
            retry_burst: 10_000,
            max_concurrent_retries: 1,
        });
        let entered = Arc::new(AtomicBool::new(false));

        let task_budget = budget.clone();
        let task_entered = entered.clone();
        let handle = tokio::spawn(async move {
            run_with_retries::<(), _, _>(Some(&task_budget), "tokenize", move |is_retry| {
                let entered = task_entered.clone();
                async move {
                    if !is_retry {
                        return Err(overloaded());
                    }
                    entered.store(true, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                    unreachable!("cancelled before completing")
                }
            })
            .await
        });

        for _ in 0..100 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(entered.load(Ordering::SeqCst), "retry attempt started");

        handle.abort();
        assert!(handle.await.expect_err("task aborted").is_cancelled());

        // The single permit must be free again; the timeout only guards a regression.
        let guard = tokio::time::timeout(Duration::from_millis(100), budget.acquire())
            .await
            .expect("permit released on cancellation");
        drop(guard);
    }

    /// Cancelling a retry while it is still WAITING for budget (concurrency permit or rate token)
    /// leaves nothing behind: the permit stays available and no token is consumed.
    #[tokio::test(start_paused = true)]
    async fn cancellation_while_waiting_for_budget_holds_nothing() {
        let budget = TokenizerRetryBudget::new(TokenizerRetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            retries_per_second: 1,
            retry_burst: 2,
            max_concurrent_retries: 1,
        });

        // Waiting on the concurrency permit: another attempt holds the only one.
        let held = budget.acquire().await;
        let waiter_budget = budget.clone();
        let waiter = tokio::spawn(async move {
            let _guard = waiter_budget.acquire().await;
            unreachable!("the permit is held for the whole test section");
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "blocked on the concurrency permit");
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(held);

        // Waiting on the rate limiter: the bucket (burst 2) has one token left; take it, then a
        // waiter needs a refill. Cancel it mid-wait: the next token must still be there for us.
        drop(budget.acquire().await);
        let waiter_budget = budget.clone();
        let waiter = tokio::spawn(async move { drop(waiter_budget.acquire().await) });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!waiter.is_finished(), "blocked on the rate limiter");
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let got = tokio::time::timeout(Duration::from_millis(10), budget.acquire()).await;
        assert!(got.is_ok(), "the refilled token was not consumed by the cancelled waiter");
    }

    #[tokio::test(start_paused = true)]
    async fn non_503_errors_are_not_retried() {
        let budget = TokenizerRetryBudget::new(fast_policy());
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let err = run_with_retries(Some(&budget), "tokenize", move |_is_retry| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TokenizerError::Status {
                    status: 400,
                    body: "bad request".to_string(),
                })
            }
        })
        .await
        .expect_err("400 is permanent");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(err, TokenizerError::Status { status: 400, .. }));

        let transport = run_with_retries(Some(&budget), "tokenize", move |_is_retry| async {
            Err::<(), _>(TokenizerError::Unmapped("m".to_string()))
        })
        .await
        .expect_err("transport-ish error is permanent");
        assert!(matches!(transport, TokenizerError::Unmapped(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn success_is_not_retried() {
        let budget = TokenizerRetryBudget::new(fast_policy());
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let value = run_with_retries(Some(&budget), "tokenize", move |_is_retry| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(7u32)
            }
        })
        .await
        .expect("first attempt succeeds");

        assert_eq!(value, 7);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn no_budget_makes_exactly_one_attempt() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let err = run_with_retries(None, "tokenize", move |_is_retry| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(overloaded())
            }
        })
        .await
        .expect_err("503 without a budget is returned");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(err, TokenizerError::Status { status: 503, .. }));
    }

    #[test]
    fn backoff_nominal_saturates_beyond_u64_nanos() {
        // initial·2 exceeds u64::MAX nanoseconds but is still below the cap: must saturate to the
        // cap, never wrap modulo 2^64 into a tiny delay.
        let policy = TokenizerRetryPolicy {
            initial_backoff: Duration::from_secs(1_000_000_000_000_000_000),
            max_backoff: Duration::MAX,
            ..TokenizerRetryPolicy::default()
        };
        assert_eq!(backoff_nominal(&policy, 2), Duration::MAX);
        assert_eq!(backoff_nominal(&policy, u32::MAX), Duration::MAX);
    }

    #[test]
    fn backoff_nominal_doubles_caps_and_never_overflows() {
        let policy = TokenizerRetryPolicy {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            ..Default::default()
        };
        assert_eq!(backoff_nominal(&policy, 1), Duration::from_millis(100));
        assert_eq!(backoff_nominal(&policy, 2), Duration::from_millis(200));
        assert_eq!(backoff_nominal(&policy, 5), Duration::from_millis(1_600));
        assert_eq!(backoff_nominal(&policy, 6), Duration::from_secs(2));
        assert_eq!(backoff_nominal(&policy, u32::MAX), Duration::from_secs(2));
    }

    #[test]
    fn equal_jitter_stays_within_bounds() {
        for nominal in [Duration::from_millis(1), Duration::from_millis(100), Duration::from_secs(2)] {
            for _ in 0..1_000 {
                let delay = equal_jitter(nominal);
                assert!(delay >= nominal / 2, "{delay:?} below half of {nominal:?}");
                assert!(delay <= nominal, "{delay:?} above {nominal:?}");
            }
        }
        assert_eq!(equal_jitter(Duration::ZERO), Duration::ZERO);
    }
}
