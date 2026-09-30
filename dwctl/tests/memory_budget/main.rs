//! Heap budgets for requests in flight.
//!
//! Each test holds a round of identical requests open at a mock upstream and
//! measures the heap the application keeps per request, at two payload sizes.
//! From the two rounds it derives:
//!
//! - `copies`: bytes held per payload byte while a request is in flight;
//! - `fixed`: bytes held per in-flight request regardless of payload size.
//!
//! Budgets cap both, so a change that keeps another copy of a body in memory
//! while the upstream is working fails here.
//!
//! The output also reports what stays allocated after requests complete
//! (`idle copies`, `leaked`). It comes from request logging's writes to the
//! database and varies between runs, so it is not budgeted.
//!
//! Connections are not reused between requests, so memory held by idle
//! keep-alive connections is not part of the measurement. The allocator counts
//! the whole process, so tests in this binary run one at a time.
//!
//! Run with `just test memory`.

mod alloc;
mod harness;
mod measure;
mod tokenizer;
mod upstream;

use std::sync::{Mutex, MutexGuard};

use axum::body::Bytes;
use serde_json::json;

use harness::{Harness, MODEL_ALIAS, Options};
use measure::{Load, Scaling, Target, round, scaling};
use upstream::Reply;

#[global_allocator]
static ALLOCATOR: alloc::CountingAllocator = alloc::CountingAllocator;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const SMALL: usize = 64 * 1024;
const LARGE: usize = 256 * 1024;
/// Inference keys allowed to call the model; routing copies the key set per request.
const API_KEYS: usize = 500;

struct Budget {
    copies: f64,
    fixed_kib: f64,
}

fn check(name: &str, scaling: &Scaling, budget: Budget) {
    println!("{name}: {scaling}");
    let fixed_kib = scaling.fixed / 1024.0;
    assert!(
        scaling.copies <= budget.copies && fixed_kib <= budget.fixed_kib,
        "{name} is over budget: copies {:.2} (budget {:.2}), fixed {fixed_kib:.1} KiB (budget {:.1} KiB)",
        scaling.copies,
        budget.copies,
        budget.fixed_kib,
    );
}

fn chat_request(content_bytes: usize, stream: bool) -> Bytes {
    let body = json!({
        "model": MODEL_ALIAS,
        "stream": stream,
        "messages": [{ "role": "user", "content": "a".repeat(content_bytes) }],
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The load generator and the mock upstream stay out of the count; every other
/// test relies on it. What remains is the idle application's own background work.
#[test]
fn load_generator_is_not_counted() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: 1 });
    let body = chat_request(LARGE, false);
    let round = round(
        &harness,
        Load {
            target: Target::Upstream,
            path: "/ai/v1/chat/completions",
            size: body.len(),
            body,
            reply: Reply::Hold,
        },
    );
    let kib = |bytes: f64| bytes / 1024.0;
    println!(
        "load generator and upstream alone: held {:.1} KiB, peak {:.1} KiB, retained {:.1} KiB per request",
        kib(round.held),
        kib(round.peak),
        kib(round.retained)
    );
    for bytes in [round.held, round.peak, round.retained] {
        assert!(kib(bytes).abs() < 4.0, "load generator counted: {round:?}");
    }
}

#[test]
fn chat_completion_request_body() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: API_KEYS });
    let scaling = scaling(
        &harness,
        |size| {
            let body = chat_request(size, false);
            Load {
                target: Target::Application,
                path: "/ai/v1/chat/completions",
                size: body.len(),
                body,
                reply: Reply::Hold,
            }
        },
        SMALL,
        LARGE,
    );
    check(
        "chat completion request body",
        &scaling,
        Budget {
            copies: 9.5,
            fixed_kib: 275.0,
        },
    );
}

#[test]
fn chat_completion_streamed_response() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: API_KEYS });
    let scaling = scaling(
        &harness,
        |size| Load {
            target: Target::Application,
            path: "/ai/v1/chat/completions",
            body: chat_request(64, true),
            reply: Reply::StreamThenHold { content_bytes: size },
            size,
        },
        SMALL,
        LARGE,
    );
    check(
        "chat completion streamed response",
        &scaling,
        Budget {
            copies: 2.0,
            fixed_kib: 185.0,
        },
    );
}
