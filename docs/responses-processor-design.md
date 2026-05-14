# `/v1/responses` daemon processor: shared fusillade machinery + custom HttpClient

## Decision

dwctl's `DwctlRequestProcessor` reuses fusillade's existing
`Request<Claimed>::process` → `Request<Processing>::complete` machinery
unchanged. The multi-step response loop is plugged in via fusillade's
`HttpClient` trait — `execute()` runs the loop and returns the assembled
response as a synthesized `HttpResponse`.

No fusillade changes. The state transitions, retry policy, persistence,
span instrumentation, and cancellation hookup are inherited from the
default-processor path.

## The seam

```rust
#[async_trait]
pub trait fusillade::HttpClient: Send + Sync + Clone {
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse>;
}
```

Fusillade's `process()` blindly spawns whatever `HttpClient` you hand it
into a tokio task and returns a `Request<Processing>` carrying the
result channel and abort handle for that task. Nothing in the contract
requires the implementation to make a literal single HTTP call — it
just has to return a response.

## The custom client

```rust
#[derive(Clone)]
pub struct ResponseLoopHttpClient<P, T> {
    response_store: Arc<FusilladeResponseStore<P>>,
    tool_executor: Arc<T>,
    inner_http: Arc<dyn onwards::HttpClient + Send + Sync>,
    tool_resolver: Option<Arc<dyn DaemonToolResolver>>,
    loop_config: LoopConfig,
}

impl fusillade::HttpClient for ResponseLoopHttpClient<…> {
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse> {
        // resolve tools → register pending input → run_response_loop → assemble →
        // finalize head sub-request row → synthesize HttpResponse
    }
}
```

`execute()` returns:
- `Ok(HttpResponse { status: 200, body: assembled_json })` on success
- `Ok(HttpResponse { status: 500, body: error_json })` on `LoopError`

Loop failures go through fusillade's normal `should_retry` predicate
rather than a custom non-retriable path — same policy as any other
upstream 500.

## The new processor

```rust
async fn process(&self, request, http, storage, should_retry, cancellation) -> Result<…> {
    if request.data.path != "/v1/responses" {
        return self.default.process(request, http, storage, should_retry, cancellation).await;
    }
    let loop_client = ResponseLoopHttpClient { /* clones of Arcs */ };
    let processing = request.process(loop_client, storage).await?;
    processing.complete(storage, should_retry, cancellation).await
}
```

Non-`/v1/responses` paths still delegate to `DefaultRequestProcessor`
unchanged.

## Lifecycle

```
daemon claims pending /v1/responses row (state: claimed)
  └─ DwctlRequestProcessor::process
        ├─ request.process(loop_client, storage)          [fusillade]
        │     ├─ tokio::spawn → loop_client.execute(...)
        │     │     ├─ register_pending_with_id
        │     │     ├─ onwards::run_response_loop
        │     │     ├─ assemble_response + finalize_head_request
        │     │     └─ return HttpResponse { status, body }
        │     ├─ persist state=processing                  [fusillade]
        │     └─ return Request<Processing> { result_rx, abort_handle }
        └─ processing.complete(should_retry, cancellation) [fusillade]
              ├─ await result_rx vs cancellation
              ├─ apply should_retry policy
              └─ persist Completed | Failed                [fusillade]
```

## Abort

`Processing::complete`'s cancellation future (daemon shutdown, user
cancel) calls `abort_handle.abort()` on the spawned task. tokio drops
the task's future at the next await, which cascades into
`run_response_loop` → `fire_model_call` → the in-flight upstream
`http_client.request(...)`. One abort cancels the entire chain of
in-flight calls.

## Resume

The parent row sits in `processing` for the entire loop, getting the
`processing_timeout_ms` budget (4h prod) instead of the 60s
`claim_timeout_ms`. Steady-state operation has no reclaim race.

After a genuine worker death, another dwctl process eventually
re-claims the row and starts a fresh `ResponseLoopHttpClient::execute`.
The persisted chain (`fusillade.response_steps`) is read by
`next_action_for`:

- **Completed steps survive.** `next_action_for` walks the chain, finds
  the terminal step, returns `Complete(payload)` — the loop assembles
  and returns.
- **In-flight (`processing`) steps fail the chain.** Re-firing risks
  duplicate model/tool side effects and the design doesn't track enough
  to know whether the upstream call completed before the worker died.
  `decide_next_action` returns `NextAction::Fail` with type
  `step_abandoned` (renamed from `transition_invariant_violation`,
  which only made sense as an internal invariant claim).

## Coupling

Fusillade has zero knowledge of dwctl. The two traits it exposes
(`RequestProcessor`, `HttpClient`) are abstract enough that:

- dwctl plugs in `DwctlRequestProcessor` + `ResponseLoopHttpClient`
  for the multi-step path, `DefaultRequestProcessor` + `ReqwestHttpClient`
  for everything else.
- A different consumer of fusillade (e.g., a pure batch-chat-completions
  service) compiles with the defaults and never touches the response-loop
  code.

The one soft coupling — rows with `path == "/v1/responses"` require the
multi-step processor — is a runtime contract, not a code dependency.
In practice every daemon worker is a dwctl process and registers the
same processor at startup, so the contract is trivially satisfied.

## Implementation checklist

1. New `dwctl/src/responses/loop_http_client.rs`: the `ResponseLoopHttpClient`
   struct, `Clone` impl, and `fusillade::HttpClient` impl with the
   `execute` body. Lift the body from the current `processor.rs`.
2. Slim `dwctl/src/responses/processor.rs` `/v1/responses` branch to the
   ~6-line form above.
3. Rename `transition_invariant_violation` → `step_abandoned` in
   `dwctl/src/responses/transition.rs`.
4. Tests:
   - Existing daemon-path tests should pass unchanged (outputs same, only
     parent row state during the loop changes).
   - Regression test: `/v1/responses` row whose loop runs >60s lands in
     `Completed`, not `Failed`.
5. No fusillade changes.
