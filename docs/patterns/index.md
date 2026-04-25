# Patterns

Recipe-style answers to common job-queue problems. Each page is one focused use case, not a survey of options.

If you're new to eddyq, read the [guide](/guide/) first — patterns assume you know what `q.enqueue`, `q.work`, and `groupKey` mean.

## Available patterns

| Pattern | When you reach for it |
|---|---|
| [Bulk enqueue across queues](./bulk-mixed-queue) | Inserting many jobs of mixed kinds in one round trip |
| [Idempotent jobs](./idempotency) | A job must not run twice for the same upstream event |
| [Throttling jobs](./throttling) | Bound the rate at which jobs hit a downstream API |
| [Stop retrying](./stop-retrying) | The job can't succeed; don't waste attempts |
| [Job timeouts](./timeouts) | Bound how long any single job can run |
| [Fail fast on Postgres outage](./fail-fast) | Connect/start should error loudly, not hang |

Suggest a pattern: [open an issue](https://github.com/EddyQueue/eddyq/issues/new).
