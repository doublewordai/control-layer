# Graceful shutdown

The standalone `onwards` binary handles SIGTERM and SIGINT using Axum's
`with_graceful_shutdown`. Applications embedding the Onwards library still
own their HTTP server and its shutdown policy.

On receipt of a signal:

1. `/readyz` on the metrics port changes from HTTP 200 to HTTP 503.
2. For `--shutdown-delay-secs` (default 5), requests continue to be served
   while load balancers observe the readiness change.
3. Axum stops accepting new proxy connections, closes idle keep-alive
   connections, and waits for accepted requests and response bodies to finish.
   An SSE response keeps the process alive until its body ends, even though
   its headers were sent earlier.
4. Metrics and `/healthz` remain available while the proxy drains. When the
   proxy finishes, the metrics server closes and tracing is flushed.

If draining exceeds `--shutdown-timeout-secs` (default 300), the process
logs the timeout and exits unsuccessfully. Remaining streams are cut off;
this is a forced termination, not a successful drain. The metrics server
has a separate five-second shutdown ceiling after the proxy stops.

Use HTTP probes on the metrics port, not TCP probes on the proxy listener:

```yaml
terminationGracePeriodSeconds: 360
containers:
  - name: onwards
    # Pin the release containing this feature, or its immutable image digest.
    args:
      - --targets
      - /etc/onwards/targets.json
      - --shutdown-delay-secs
      - "5"
      - --shutdown-timeout-secs
      - "300"
    readinessProbe:
      httpGet:
        path: /readyz
        port: 9090
      periodSeconds: 2
      failureThreshold: 1
    livenessProbe:
      httpGet:
        path: /healthz
        port: 9090
```

The pod's termination budget must exceed the propagation delay, proxy drain,
metrics shutdown and tracing flush. Set it using the request durations your
service supports. The example allows 55 seconds beyond the default 305-second
propagation-plus-drain budget. The propagation delay replaces a sleep-only
`preStop` hook; adding such a hook would consume extra time before SIGTERM.
The health routes reveal only process readiness/liveness, are unauthenticated,
and should remain internal alongside metrics. Readiness does not test every
configured upstream model.

Do not apply a normal rolling update to old gateway versions that lack this
behavior while they have active streams. Create a separate deployment and
Service, move callers to its distinct hostname without restarting them,
then verify the old gateways have no new requests and zero in-flight work
before retiring them. Merely changing a Service selector does not move
established connections. Validate client configuration propagation and retry
behavior in a rehearsal before using this procedure in production.

After all replicas support draining, use required pod anti-affinity on
`kubernetes.io/hostname`, a suitable PodDisruptionBudget, and enough spare
capacity for rolling updates. These controls and signal handling do not
preserve TCP streams through an abrupt node failure.

The Unix integration tests start the actual binary and a gated HTTP backend.
They cover SSE completion across SIGTERM, a unary request still waiting for
headers, deadline expiry with a stuck stream, and idle HTTP/1.1 keep-alive
closure on SIGINT. They use synthetic traffic and do not terminate production
pods. Run them with:

```bash
cargo test --package onwards --test graceful_shutdown
```

See [Axum's server API](https://docs.rs/axum/latest/axum/serve/struct.Serve.html#method.with_graceful_shutdown)
and [Kubernetes pod termination](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/#pod-termination).
