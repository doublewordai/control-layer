//! Heap budgets for requests in flight.
//!
//! Each test holds a round of identical requests open at one point in their
//! life and measures the heap the application keeps per request, at two
//! payload sizes:
//!
//! - a request body, while the upstream works on it;
//! - a streamed response, after its output has reached the client and before
//!   the stream finishes;
//! - a complete response, while the client has not read it yet.
//!
//! Payloads are shaped like everyday traffic (see `payload`), because how much
//! the application holds per byte depends on a body's structure as well as its
//! size. From the two rounds each test derives:
//!
//! - `copies`: bytes held per payload byte while a request is in flight;
//! - `fixed`: bytes held per in-flight request regardless of payload size.
//!
//! Budgets cap both, so a change that keeps another copy of a body in memory
//! while a request is in flight fails here.
//!
//! The output also reports what stays allocated after requests complete
//! (`idle copies`, `leaked`). It comes from request logging's writes to the
//! database and varies between runs, so it is not budgeted.
//!
//! How much the application holds depends on the pieces its reads and writes
//! move, so the harness fixes them: request bodies arrive through a small
//! receive buffer, streamed events arrive one at a time, and responses leave
//! through small send and receive buffers. Connections are not reused between
//! requests, so memory held by idle keep-alive connections is not part of the
//! measurement. The allocator counts the whole process, so tests in this
//! binary run one at a time.
//!
//! Run with `just test memory`.

mod alloc;
mod harness;
mod measure;
mod payload;
mod tokenizer;
mod upstream;

use std::sync::{Mutex, MutexGuard};

use harness::{Harness, MODEL_ALIAS, Options, UPSTREAM_MODEL};
use measure::{Load, Scaling, Target, round, scaling};
use upstream::Reply;

#[global_allocator]
static ALLOCATOR: alloc::CountingAllocator = alloc::CountingAllocator;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Request bodies, in bytes: a typical agent request and a long one.
const REQUEST_BYTES: [usize; 2] = [96 * 1024, 300 * 1024];
/// Streamed output, in tokens: a typical response and a long one.
const STREAMED_TOKENS: [usize; 2] = [256, 2304];
/// Complete output, in tokens. A typical complete response fits in socket
/// buffers and leaves the application at once, so only long ones are held
/// while the client reads them.
const COMPLETE_TOKENS: [usize; 2] = [8192, 32768];
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

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The load generator and the mock upstream stay out of the count; every other
/// test relies on it. What remains is the idle application's own background work.
#[test]
fn load_generator_is_not_counted() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: 1 });
    let body = payload::chat_request(MODEL_ALIAS, REQUEST_BYTES[1], false);
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

/// A request body, held while the upstream works on it.
#[test]
fn chat_completion_request_body() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: API_KEYS });
    let scaling = scaling(
        &harness,
        |size| {
            let body = payload::chat_request(MODEL_ALIAS, size, false);
            Load {
                target: Target::Application,
                path: "/ai/v1/chat/completions",
                size: body.len(),
                body,
                reply: Reply::Hold,
            }
        },
        REQUEST_BYTES,
    );
    check(
        "chat completion request body",
        &scaling,
        Budget {
            copies: 10.35,
            fixed_kib: 365.0,
        },
    );
}

/// A streamed response, held after its output has reached the client and
/// before the stream finishes. Sizes are the bytes of the events delivered.
#[test]
fn chat_completion_streamed_response() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: API_KEYS });
    let scaling = scaling(
        &harness,
        |tokens| Load {
            target: Target::Application,
            path: "/ai/v1/chat/completions",
            body: payload::chat_request(MODEL_ALIAS, 0, true),
            reply: Reply::StreamThenHold { tokens },
            size: payload::stream(UPSTREAM_MODEL, tokens).content_bytes(),
        },
        STREAMED_TOKENS,
    );
    check(
        "chat completion streamed response",
        &scaling,
        Budget {
            copies: 1.25,
            fixed_kib: 150.0,
        },
    );
}

/// A complete response, held while the client has not read it yet. Sizes are
/// the bytes of the upstream's response body.
#[test]
fn chat_completion_response() {
    let _one_at_a_time = one_at_a_time();
    let harness = Harness::start(Options { api_keys: API_KEYS });
    let scaling = scaling(
        &harness,
        |tokens| Load {
            target: Target::Application,
            path: "/ai/v1/chat/completions",
            body: payload::chat_request(MODEL_ALIAS, 0, false),
            reply: Reply::Complete { tokens },
            size: payload::completion(UPSTREAM_MODEL, tokens).len(),
        },
        COMPLETE_TOKENS,
    );
    check(
        "chat completion response",
        &scaling,
        Budget {
            copies: 2.0,
            fixed_kib: 70.0,
        },
    );
}
