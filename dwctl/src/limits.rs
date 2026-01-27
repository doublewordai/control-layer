//! Resource limiting for protecting system capacity.
//!
//! This module provides rate limiting and concurrency control mechanisms
//! to prevent resource exhaustion under high load.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{FileLimitsConfig, LimitsConfig};
use crate::errors::{Error, Result};

/// Container for all resource limiters.
///
/// This struct holds all the individual limiters used by the application.
/// Add new limiters here as fields when implementing additional rate limiting.
#[derive(Debug, Default, Clone)]
pub struct Limiters {
    /// Limiter for concurrent file uploads. None means unlimited.
    pub file_uploads: Option<Arc<UploadLimiter>>,
}

impl Limiters {
    /// Creates all limiters from configuration.
    pub fn new(config: &LimitsConfig) -> Self {
        Self {
            file_uploads: UploadLimiter::new(&config.files).map(Arc::new),
        }
    }
}

/// Controls concurrent file upload capacity.
///
/// This limiter implements a bounded queue with configurable concurrency,
/// waiting capacity, and timeout. When limits are exceeded, requests
/// receive HTTP 429 (Too Many Requests).
#[derive(Debug)]
pub struct UploadLimiter {
    /// Semaphore controlling max concurrent uploads
    semaphore: Arc<Semaphore>,
    /// Current number of requests waiting for a permit
    waiting_count: AtomicUsize,
    /// Maximum allowed waiting requests (None = unlimited)
    max_waiting: Option<usize>,
    /// Maximum time to wait for a permit
    max_wait: Duration,
}

impl UploadLimiter {
    /// Creates a new upload limiter from configuration.
    ///
    /// If `max_concurrent_uploads` is 0, returns `None` (unlimited uploads).
    /// If `max_waiting_uploads` is 0, unlimited waiting is allowed.
    pub fn new(config: &FileLimitsConfig) -> Option<Self> {
        if config.max_concurrent_uploads == 0 {
            return None;
        }

        Some(Self {
            semaphore: Arc::new(Semaphore::new(config.max_concurrent_uploads)),
            waiting_count: AtomicUsize::new(0),
            // 0 means unlimited waiting queue
            max_waiting: if config.max_waiting_uploads == 0 {
                None
            } else {
                Some(config.max_waiting_uploads)
            },
            max_wait: Duration::from_secs(config.max_upload_wait_secs),
        })
    }

    /// Attempts to acquire a permit for file upload.
    ///
    /// Returns `Ok(UploadPermit)` if a slot is available or becomes available
    /// within the timeout. Returns `Err(TooManyRequests)` if:
    /// - The waiting queue is full (`max_waiting` reached)
    /// - The timeout expires before a slot becomes available
    pub async fn acquire(&self) -> Result<UploadPermit> {
        // Try to acquire immediately without waiting
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                return Ok(UploadPermit { _permit: permit });
            }
            Err(_) => {
                // No permit available, need to wait
            }
        }

        // Check if we can join the waiting queue
        let current_waiting = self.waiting_count.fetch_add(1, Ordering::SeqCst);
        if let Some(max_waiting) = self.max_waiting
            && current_waiting >= max_waiting
        {
            // Queue is full, reject immediately
            self.waiting_count.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::TooManyRequests {
                message: "Too many file uploads in progress. Please retry later.".to_string(),
            });
        }

        // Optimization: try to acquire again now that we're in the waiting queue.
        // A permit may have been released between the first try_acquire and incrementing
        // waiting_count, allowing us to acquire immediately without waiting.
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                self.waiting_count.fetch_sub(1, Ordering::SeqCst);
                return Ok(UploadPermit { _permit: permit });
            }
            Err(_) => {
                // Still no permit available, proceed to wait
            }
        }

        // Wait for a permit with timeout
        let result = if self.max_wait.is_zero() {
            // Zero timeout means reject immediately if not available
            Err(Error::TooManyRequests {
                message: "Too many file uploads in progress. Please retry later.".to_string(),
            })
        } else {
            match tokio::time::timeout(self.max_wait, self.semaphore.clone().acquire_owned()).await {
                Ok(Ok(permit)) => Ok(UploadPermit { _permit: permit }),
                Ok(Err(_)) => {
                    // Semaphore closed (shouldn't happen in normal operation)
                    Err(Error::TooManyRequests {
                        message: "Upload service temporarily unavailable.".to_string(),
                    })
                }
                Err(_) => {
                    // Timeout elapsed
                    Err(Error::TooManyRequests {
                        message: "Timed out waiting for upload slot. Please retry later.".to_string(),
                    })
                }
            }
        };

        // Decrement waiting count regardless of outcome
        self.waiting_count.fetch_sub(1, Ordering::SeqCst);

        result
    }
}

/// RAII guard that releases the upload permit when dropped.
///
/// This uses an owned permit so it can be held across await points
/// and moved between tasks if needed.
#[must_use]
pub struct UploadPermit {
    _permit: OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(max_concurrent: usize, max_waiting: usize, max_wait_secs: u64) -> FileLimitsConfig {
        FileLimitsConfig {
            max_concurrent_uploads: max_concurrent,
            max_waiting_uploads: max_waiting,
            max_upload_wait_secs: max_wait_secs,
            ..Default::default()
        }
    }

    #[test]
    fn test_unlimited_returns_none() {
        let config = test_config(0, 20, 60);
        assert!(UploadLimiter::new(&config).is_none());
    }

    #[tokio::test]
    async fn test_acquire_when_available() {
        let config = test_config(2, 10, 60);
        let limiter = UploadLimiter::new(&config).unwrap();

        // Should acquire immediately
        let permit1 = limiter.acquire().await;
        assert!(permit1.is_ok());

        let permit2 = limiter.acquire().await;
        assert!(permit2.is_ok());
    }

    #[tokio::test]
    async fn test_acquire_waits_and_succeeds() {
        let config = test_config(1, 10, 5);
        let limiter = Arc::new(UploadLimiter::new(&config).unwrap());

        // Take the only slot
        let permit1 = limiter.acquire().await.unwrap();

        // Spawn a task that will wait
        let limiter_clone = limiter.clone();
        let handle = tokio::spawn(async move { limiter_clone.acquire().await });

        // Give time for the waiter to start waiting
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release the permit
        drop(permit1);

        // Waiter should succeed
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_acquire_rejects_when_queue_full() {
        let config = test_config(1, 1, 60);
        let limiter = Arc::new(UploadLimiter::new(&config).unwrap());

        // Take the only slot
        let _permit1 = limiter.acquire().await.unwrap();

        // First waiter joins queue
        let limiter_clone = limiter.clone();
        let _handle1 = tokio::spawn(async move { limiter_clone.acquire().await });

        // Give time for waiter to enter queue
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Second waiter should be rejected (queue full)
        let result = limiter.acquire().await;
        assert!(result.is_err());
        if let Err(Error::TooManyRequests { message }) = result {
            assert!(message.contains("Too many file uploads"));
        } else {
            panic!("Expected TooManyRequests error");
        }
    }

    #[tokio::test]
    async fn test_acquire_times_out() {
        let config = test_config(1, 10, 1); // 1 second timeout
        let limiter = Arc::new(UploadLimiter::new(&config).unwrap());

        // Take the only slot and hold it
        let _permit1 = limiter.acquire().await.unwrap();

        // Try to acquire with timeout - should fail after 1 second
        let start = std::time::Instant::now();
        let result = limiter.acquire().await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(elapsed >= Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(2));

        if let Err(Error::TooManyRequests { message }) = result {
            assert!(message.contains("Timed out"));
        } else {
            panic!("Expected TooManyRequests error");
        }
    }

    #[tokio::test]
    async fn test_zero_wait_rejects_immediately() {
        let config = test_config(1, 10, 0); // 0 second timeout = reject immediately
        let limiter = UploadLimiter::new(&config).unwrap();

        // Take the only slot
        let _permit1 = limiter.acquire().await.unwrap();

        // Should reject immediately
        let start = std::time::Instant::now();
        let result = limiter.acquire().await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(elapsed < Duration::from_millis(100)); // Should be nearly instant
    }

    #[tokio::test]
    async fn test_permit_released_on_drop() {
        let config = test_config(1, 10, 1);
        let limiter = UploadLimiter::new(&config).unwrap();

        {
            let _permit = limiter.acquire().await.unwrap();
            // permit dropped here
        }

        // Should be able to acquire again
        let result = limiter.acquire().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unlimited_waiting_queue() {
        // max_waiting=0 means unlimited waiting queue
        let config = test_config(1, 0, 5);
        let limiter = Arc::new(UploadLimiter::new(&config).unwrap());

        // Take the only slot
        let permit1 = limiter.acquire().await.unwrap();

        // Spawn multiple waiters - should all be allowed to wait
        let mut handles = vec![];
        for _ in 0..10 {
            let limiter_clone = limiter.clone();
            handles.push(tokio::spawn(async move { limiter_clone.acquire().await }));
        }

        // Give time for waiters to enter queue
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release the permit - first waiter should succeed
        drop(permit1);

        // At least the first waiter should succeed
        let result = handles.remove(0).await.unwrap();
        assert!(result.is_ok());
    }
}
