import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { EmailModule } from "./email/email.module.js";
import { ReportsModule } from "./reports/reports.module.js";

const DEFAULT_DATABASE_URL =
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

/**
 * Worker composition root. Imports the same feature modules as AppModule —
 * Nest's DI still instantiates the controllers, but without an HTTP listener
 * they're inert. Importing the same modules keeps feature boundaries intact
 * (email lives entirely under `src/email/`), which matters more as the app
 * grows.
 *
 * `EDDYQ_SUBSCRIBE_TO` lets you deploy differently-shaped worker fleets from
 * the same image.
 */
const SUBSCRIBE_TO = (process.env.EDDYQ_SUBSCRIBE_TO ?? "default")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.EDDYQ_DATABASE_URL ?? DEFAULT_DATABASE_URL,
      workerConcurrency: Number(process.env.EDDYQ_WORKER_CONCURRENCY ?? 10),
      subscribeTo: SUBSCRIBE_TO,
      // Local-dev convenience — in production, run migrations as a deploy step.
      runMigrations: process.env.EDDYQ_RUN_MIGRATIONS === "true",
    }),
    EmailModule,
    ReportsModule,
  ],
})
export class WorkersModule {}
