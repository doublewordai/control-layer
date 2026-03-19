# Event Capture System

## Overview

The control layer needs to capture business events (user sign-up, batch creation, API key creation) and forward them to analytics and CRM providers. Since the control layer is open source, provider-specific integration logic must not live in this codebase.

The solution leverages the existing OpenTelemetry tracing infrastructure. A custom OTel Collector sits between dwctl and the trace backend, filtering request spans that match configured business events and dispatching them to downstream providers.

## Architecture

```
dwctl                        OTel Collector (custom, Go)              Backends
─────                        ──────────────────────────              ────────
OTLP spans ──────────►  ┌─ pipeline/traces ──────────────► Trace backend
(existing, just           │                                   (unchanged)
 retarget endpoint)       │
                          └─ pipeline/events
                               filter: route + method + status
                               ├──────────────────────────► Analytics provider
                               └──────────────────────────► CRM provider
```

### Why this approach

- **Minimal open-source footprint** — 3 lines added to dwctl (a single span attribute)
- **Clean separation** — dwctl emits standard OTel traces; the collector owns all provider logic
- **No new infrastructure in dwctl** — no Redis, no message queue, no new dependencies
- **Trace backend unaffected** — collector forwards all traces unchanged, just adds a second pipeline
- **Extensible** — adding new events is a config rule in the collector, not a code change

### Alternatives considered

**Redis Streams**: dwctl publishes structured events to a Redis Stream, a separate consumer reads and fans out. Rejected because it requires a new dependency (redis crate), a new module, config structs, and Redis infrastructure in the Helm chart — significantly more code in the open-source repo for the same outcome.

**PostgreSQL LISTEN/NOTIFY**: Emit events via the existing PG notification system. Rejected because it would bake analytics dispatch logic directly into the open-source backend code.

## Changes required

| Repo | Change | Size |
|------|--------|------|
| `control-layer` (dwctl) | Add `user.id` span attribute on auth success | 3 lines |
| `internal` | Deploy OTel Collector, retarget dwctl OTLP endpoint | Config only |
| New: `otel-collector` | Custom collector distribution with event exporter | New Go repo |

---

## 1. dwctl — Add user.id to request spans

**File:** `dwctl/src/auth/current_user.rs`

The `FromRequestParts` implementation for `CurrentUser` has three auth success paths (API key, JWT session, proxy header). After each, add one line to record the authenticated user's ID on the current request span:

```rust
tracing::Span::current().set_attribute("user.id", user.id.to_string());
```

This uses the existing `OpenTelemetrySpanExt::set_attribute` method already used in `lib.rs` for HTTP semantic conventions. The attribute attaches to the parent request span created by the TraceLayer. No new dependencies, modules, or config changes.

### Span data available for event matching

Every authenticated request span carries:

| Attribute | Source | Example |
|-----------|--------|---------|
| `http.request.method` | TraceLayer | `POST` |
| `http.route` | TraceLayer | `/admin/api/v1/users` |
| `http.response.status_code` | TraceLayer | `201` |
| `url.path` | TraceLayer | `/admin/api/v1/users` |
| `user.id` | Auth extractor (**new**) | `a1b2c3d4-...` |
| `api.type` | TraceLayer | `admin` |

### Event mapping rules

These rules are configured in the collector, not in dwctl:

| Rule | Event name |
|------|------------|
| `POST /admin/api/v1/users` + 2xx | `user.created` |
| `POST /ai/v1/batches` + 2xx | `batch.created` |
| `POST /admin/api/v1/users/:user_id/api-keys` + 2xx | `api_key.created` |

The `url.path` can be parsed for resource IDs (e.g., the `:user_id` segment) when needed.

---

## 2. Custom OTel Collector (closed source)

A custom OTel Collector distribution built with the OTel Collector Builder (`ocb`). It includes the standard OTLP receiver and HTTP exporter, plus a custom event exporter.

### Repository structure

```
otel-collector/
├── cmd/
│   └── collector/
│       └── main.go              # ocb-generated entrypoint
├── exporter/
│   └── eventexporter/
│       ├── config.go            # Event rules + provider configs
│       ├── factory.go           # OTel component factory
│       ├── exporter.go          # Core: filter spans → map to events → dispatch
│       └── exporter_test.go
├── providers/
│   ├── provider.go              # Provider interface
│   ├── analytics.go             # Analytics provider (HTTP capture API)
│   └── crm.go                   # CRM provider (events API)
├── builder-config.yaml          # ocb manifest
├── config.yaml                  # Example collector configuration
├── Dockerfile
└── README.md
```

### Builder config

```yaml
# builder-config.yaml
dist:
  name: dw-otel-collector
  output_path: ./cmd/collector

exporters:
  - gomod: "github.com/doublewordai/otel-collector/exporter/eventexporter v0.1.0"
  - gomod: "go.opentelemetry.io/collector/exporter/otlphttpexporter v0.120.0"

receivers:
  - gomod: "go.opentelemetry.io/collector/receiver/otlpreceiver v0.120.0"

processors:
  - gomod: "go.opentelemetry.io/collector/processor/batchprocessor v0.120.0"
```

### Collector configuration

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  # Forward all traces to the trace backend (existing behavior, unchanged)
  otlphttp/traces:
    endpoint: ${TRACE_BACKEND_ENDPOINT}
    headers:
      Authorization: ${TRACE_BACKEND_AUTH}

  # Custom event exporter — filters spans and dispatches to providers
  events:
    rules:
      - name: user.created
        method: POST
        route: /admin/api/v1/users
        status_min: 200
        status_max: 299
      - name: batch.created
        method: POST
        route: /ai/v1/batches
        status_min: 200
        status_max: 299
      - name: api_key.created
        method: POST
        route_pattern: "^/admin/api/v1/users/.+/api-keys$"
        status_min: 200
        status_max: 299
    providers:
      analytics:
        enabled: true
        host: ${ANALYTICS_HOST}
        api_key: ${ANALYTICS_API_KEY}
        distinct_id_attribute: user.id
      crm:
        enabled: false
        api_key: ${CRM_API_KEY}

service:
  pipelines:
    traces/backend:
      receivers: [otlp]
      exporters: [otlphttp/traces]
    traces/events:
      receivers: [otlp]
      exporters: [events]
```

### Custom exporter logic

The exporter implements the `ConsumeTraces` interface. It receives all trace data, walks each span, and checks against configured rules:

```go
func (e *eventExporter) ConsumeTraces(ctx context.Context, td ptrace.Traces) error {
    for i := 0; i < td.ResourceSpans().Len(); i++ {
        rs := td.ResourceSpans().At(i)
        for j := 0; j < rs.ScopeSpans().Len(); j++ {
            ss := rs.ScopeSpans().At(j)
            for k := 0; k < ss.Spans().Len(); k++ {
                span := ss.Spans().At(k)
                attrs := span.Attributes()

                method := getStringAttr(attrs, "http.request.method")
                route  := getStringAttr(attrs, "http.route")
                status := getIntAttr(attrs, "http.response.status_code")
                userID := getStringAttr(attrs, "user.id")

                for _, rule := range e.config.Rules {
                    if rule.Matches(method, route, status) {
                        event := Event{
                            Name:       rule.Name,
                            DistinctID: userID,
                            Timestamp:  span.StartTimestamp().AsTime(),
                            Properties: extractProperties(attrs, span),
                        }
                        e.dispatch(ctx, event)
                    }
                }
            }
        }
    }
    return nil
}
```

### Provider interface

```go
type EventProvider interface {
    Name() string
    Send(ctx context.Context, event Event) error
}

type Event struct {
    Name       string
    DistinctID string
    Timestamp  time.Time
    Properties map[string]string
}
```

Each provider implements this interface and is independently toggleable via config. Adding a new provider requires one new file implementing the interface plus a config entry.

---

## 3. Deployment — Retarget dwctl OTLP endpoint

The OTel Collector is deployed as a Deployment + Service in the control-layer namespace. dwctl's OTLP endpoint is retargetted from the trace backend to the in-cluster collector:

```yaml
# Before (dwctl exports directly to trace backend)
env:
  OTEL_EXPORTER_OTLP_ENDPOINT: "https://trace-backend.example.com/otlp"
  OTEL_EXPORTER_OTLP_HEADERS: "Authorization=Basic%20<token>"

# After (dwctl exports to collector, collector forwards to trace backend)
env:
  OTEL_EXPORTER_OTLP_ENDPOINT: "http://otel-collector:4318"
```

The trace backend auth credentials move from dwctl's config to the collector's secrets. From the trace backend's perspective, nothing changes — it receives the same spans in the same format.

No changes to the control-layer Helm chart are required. The chart already supports OTLP configuration via environment variables.

---

## Implementation order

1. **dwctl** — Add `user.id` span attribute (3 lines, PR to control-layer)
2. **otel-collector** — Build custom collector with event exporter (new repo, parallel with step 1)
3. **Deployment** — Deploy collector, retarget dwctl OTLP endpoint, move trace backend auth to collector secrets

## Verification

1. **dwctl** — `cargo build` and `just test rust` pass. No behavioural change, just an extra span attribute.
2. **Collector** — Run locally with a test OTLP payload, verify events dispatched to provider sandbox.
3. **End-to-end** — Deploy to staging, create a user via the dashboard, verify:
   - Traces still appear in the trace backend (forwarding pipeline works)
   - `user.created` event appears in the analytics provider with correct `distinct_id`
   - Existing alerting/dashboards are unaffected

## Adding new events

To capture a new business event (e.g., `model.created`, `user.deleted`):

1. Identify the HTTP route, method, and success status codes
2. Add a rule to the collector's config
3. Restart/redeploy the collector

No code changes to dwctl required — any span that matches the route/method/status pattern becomes a capturable event automatically.
