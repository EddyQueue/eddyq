import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { EmailModule } from "./email/email.module.js";
import { ReportsModule } from "./reports/reports.module.js";

const DEFAULT_DATABASE_URL =
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

/**
 * API composition root. Imports feature modules so controllers are
 * registered, but `autoStart: false` keeps the worker runtime off — API
 * pods enqueue and serve admin reads; worker pods (see workers.module.ts)
 * actually process jobs.
 */
@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.EDDYQ_DATABASE_URL ?? DEFAULT_DATABASE_URL,
      autoStart: false,
    }),
    EmailModule,
    ReportsModule,
  ],
})
export class AppModule {}
