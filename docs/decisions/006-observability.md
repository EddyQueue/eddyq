# 006 — Observability: tracing + OpenTelemetry + metrics facade
Status: Accepted
Date: 2026-04-21

## Context

Queue observability drives adoption. Users want: metrics for dashboards (queue depth, throughput, failure rate), traces that connect enqueue spans to execution spans across process boundaries, and structured logs that don't require parsing custom formats.

## Decision

Three layers:

1. **Structured logging and spans via `tracing`.** All internal operations (`fetch`, `claim`, `execute`, `complete`, `fail`, `retry`, `sweep`) are instrumented. Span context flows through async boundaries.
2. **OpenTelemetry bridge via `tracing-opentelemetry`.** Trace context is serialized into the job row at enqueue time and restored at execution time — a job span becomes a child of the request span that enqueued it. Exporters (OTLP, Jaeger, Datadog) are user-configurable.
3. **Metrics via the `metrics` facade crate** with `metrics-exporter-prometheus` as the shipped default. The facade means users can plug in StatsD, Datadog, or their custom backend without us knowing or caring.

Metric names (conforming to Prometheus convention):

- `eddyq_jobs_enqueued_total{kind}`
- `eddyq_jobs_completed_total{kind}`
- `eddyq_jobs_failed_total{kind}`
- `eddyq_fetch_duration_seconds{quantile}` (histogram)
- `eddyq_queue_depth{state}` (gauge)
- `eddyq_group_running{group}` (gauge, added in Phase 2)

## Alternatives considered

- **`log` crate only** — Too coarse; no spans, no structured fields. Fine for a 100-LOC utility, wrong for a production queue.
- **Direct `prometheus` crate** (no facade) — Couples every metric recorder to Prometheus. Users who want Datadog or StatsD have to fork or patch.
- **OpenTelemetry-first (no tracing facade)** — Locks us to the OpenTelemetry SDK, which is heavier. tracing + tracing-opentelemetry gives us the light option for users who don't want OTel.
- **Custom event log** — Reinvents the wheel and doesn't integrate with users' existing observability stacks.

## Consequences

- Default Prometheus exporter is opt-in: `eddyq::metrics::init_prometheus(addr)` mounts a `/metrics` endpoint. Users who don't want Prometheus pay zero runtime cost.
- OpenTelemetry integration requires the user to initialize their SDK; we provide a helper for the common case (OTLP over HTTP to Tempo/Jaeger).
- Trace context is stored in the `eddyq_jobs.payload_trace` JSONB column (or similar). Format: W3C Trace Context headers.
- Users who run neither Prometheus nor OpenTelemetry still get `tracing` spans in their logs — useful for debugging even without a full observability stack.
