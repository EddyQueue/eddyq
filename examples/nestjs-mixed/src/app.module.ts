import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { PaymentsModule } from "./payments/payments.module.js";
import { WebhooksModule } from "./webhooks/webhooks.module.js";

/**
 * One Nest app, two backends. `webhooks` → Redis (high-throughput,
 * ephemeral), `payments` → Postgres (transactional, durable).
 *
 * Both backends are connected at boot; both worker runtimes start;
 * `@InjectQueue('webhooks')` and `@InjectQueue('payments')` each return a
 * handle that routes to the correct backend transparently.
 */
@Module({
  imports: [
    EddyqModule.forRoot({
      // Both backends configured = `EddyqApp` under the hood.
      databaseUrl:
        process.env.EDDYQ_DATABASE_URL ??
        "postgres://eddyq:eddyq@localhost:5433/eddyq_dev",
      redis: {
        url: process.env.REDIS_URL ?? "redis://127.0.0.1:6381",
        line: "nest-mixed",
      },
      // Routing table — queues not listed here fall back to `defaultProvider`.
      queues: [
        { name: "webhooks", provider: "redis" },
        { name: "payments", provider: "postgres" },
      ],
      defaultProvider: "postgres",
      // Local-dev convenience: apply PG migrations on boot. In production
      // run them as a deploy step (the Redis side has no migrations).
      runMigrations: process.env.EDDYQ_RUN_MIGRATIONS === "true",
    }),
    WebhooksModule,
    PaymentsModule,
  ],
})
export class AppModule {}
