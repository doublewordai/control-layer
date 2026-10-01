//! Measures the application's heap while a fixed number of requests are held
//! open at the mock upstream.

use std::time::{Duration, Instant};

use axum::body::Bytes;

use crate::alloc;
use crate::harness::Harness;
use crate::upstream::Reply;

/// Requests in flight in every round.
pub const CONCURRENCY: usize = 32;

/// Where the load generator sends requests.
#[derive(Clone, Copy)]
pub enum Target {
    Application,
    /// Straight to the mock upstream, bypassing the application.
    Upstream,
}

/// One request shape and what the upstream does with it.
pub struct Load {
    pub target: Target,
    pub path: &'static str,
    pub body: Bytes,
    pub reply: Reply,
    /// The payload size this round is reported against.
    pub size: usize,
}

/// Heap per request for one round of concurrent requests, relative to the
/// application just before the round.
#[derive(Clone, Copy, Debug)]
pub struct Round {
    pub size: usize,
    /// While every request is held at the upstream.
    pub held: f64,
    /// Highest point before the requests were released.
    pub peak: f64,
    /// Once every request has completed and the application has settled.
    pub retained: f64,
}

/// How the application's heap grows with payload size.
#[derive(Clone, Copy, Debug)]
pub struct Scaling {
    /// Bytes held per payload byte while a request is in flight.
    pub copies: f64,
    /// Bytes held per in-flight request regardless of payload size.
    pub fixed: f64,
    /// Bytes per payload byte, per concurrent request, still allocated after
    /// the first round at the larger size.
    pub idle_copies: f64,
    /// Bytes per request still allocated after a round at a size the
    /// application has already handled.
    pub leaked: f64,
    pub small: Round,
    pub large: Round,
}

impl std::fmt::Display for Scaling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kib = |bytes: f64| bytes / 1024.0;
        write!(
            f,
            "copies {:.2}, fixed {:.1} KiB, idle copies {:.2}, leaked {:.1} KiB per request \
             (held {:.1}/{:.1} KiB, peak {:.1}/{:.1} KiB per request at {:.0}/{:.0} KiB)",
            self.copies,
            kib(self.fixed),
            self.idle_copies,
            kib(self.leaked),
            kib(self.small.held),
            kib(self.large.held),
            kib(self.small.peak),
            kib(self.large.peak),
            kib(self.small.size as f64),
            kib(self.large.size as f64),
        )
    }
}

/// Runs two rounds at each size. The first round at a size lets pools,
/// caches and buffers grow to it; the second is the one measured.
pub fn scaling(harness: &Harness, load: impl Fn(usize) -> Load, small: usize, large: usize) -> Scaling {
    round(harness, load(small));
    let small = round(harness, load(small));
    let first_large = round(harness, load(large));
    let large = round(harness, load(large));
    let size_delta = (large.size - small.size) as f64;
    let copies = (large.held - small.held) / size_delta;
    Scaling {
        copies,
        fixed: small.held - copies * small.size as f64,
        idle_copies: first_large.retained / size_delta,
        leaked: small.retained.max(large.retained),
        small,
        large,
    }
}

pub fn round(harness: &Harness, load: Load) -> Round {
    let upstream = &harness.upstream;
    upstream.prepare(load.reply, CONCURRENCY);

    let baseline = settle();
    alloc::reset_peak();

    let url = match load.target {
        Target::Application => format!("{}{}", harness.base_url, load.path),
        Target::Upstream => format!("{}/chat/completions", upstream.base_url),
    };
    let key = harness.api_key.clone();
    let client = harness.client.clone();
    let body = load.body;
    let delivered = upstream.delivered();
    let requests = harness.driver.spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..CONCURRENCY {
            let (client, url, key, body) = (client.clone(), url.clone(), key.clone(), body.clone());
            let delivered = delivered.clone();
            tasks.spawn(async move {
                let mut response = client
                    .post(url)
                    .bearer_auth(key)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .expect("send request");
                let status = response.status();
                let mut error = Vec::new();
                let mut previous = 0u8;
                while let Some(chunk) = response.chunk().await.expect("read response") {
                    // A blank line ends a server-sent event.
                    let mut events = 0;
                    for &byte in chunk.iter() {
                        if byte == b'\n' && previous == b'\n' {
                            events += 1;
                        }
                        previous = byte;
                    }
                    delivered.add(events);
                    if !status.is_success() && error.len() < 2048 {
                        error.extend_from_slice(&chunk);
                    }
                }
                (status, String::from_utf8_lossy(&error).into_owned())
            });
        }
        tasks.join_all().await
    });

    let streaming = matches!(load.reply, Reply::StreamThenHold { .. });
    wait_until("every request to reach the upstream", Duration::from_secs(60), || {
        upstream.parked() >= CONCURRENCY && (!streaming || upstream.streamed() >= CONCURRENCY)
    });
    let held = settle() - baseline;
    let peak = alloc::peak_bytes() - baseline;
    assert_eq!(upstream.parked(), CONCURRENCY, "requests other than the load reached the upstream");

    upstream.release_all();
    let results = harness.driver.block_on(requests).expect("request tasks");
    for (status, error) in &results {
        assert!(status.is_success(), "request failed with {status}: {error}");
    }
    let retained = settle() - baseline;

    let per_request = |bytes: isize| bytes as f64 / CONCURRENCY as f64;
    Round {
        size: load.size,
        held: per_request(held),
        peak: per_request(peak),
        retained: per_request(retained),
    }
}

/// Waits until live bytes stop moving, then returns them. Background work
/// (routing sync, log flushing) keeps allocating a little, so "stopped" means
/// the value stays within a small band for a short window.
fn settle() -> isize {
    const WINDOW: usize = 12;
    const INTERVAL: Duration = Duration::from_millis(25);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut recent = Vec::with_capacity(WINDOW);
    loop {
        recent.push(alloc::live_bytes());
        if recent.len() > WINDOW {
            recent.remove(0);
        }
        if recent.len() == WINDOW {
            let (low, high) = (recent.iter().min().unwrap(), recent.iter().max().unwrap());
            let band = (high.abs() / 200).max(64 * 1024);
            if high - low <= band || Instant::now() > deadline {
                return recent[WINDOW - 1];
            }
        }
        std::thread::sleep(INTERVAL);
    }
}

fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out after {timeout:?} waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
