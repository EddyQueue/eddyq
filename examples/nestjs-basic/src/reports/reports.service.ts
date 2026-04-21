import { Injectable, Logger, type OnApplicationBootstrap } from "@nestjs/common";

import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

/**
 * Registers recurring schedules on bootstrap. `addSchedule` is an idempotent
 * upsert keyed on `name`, so re-registering on every boot is fine and
 * intended — schedule state (last_run_at, enabled) lives in Postgres.
 */
@Injectable()
export class ReportsService implements OnApplicationBootstrap {
  private readonly logger = new Logger(ReportsService.name);

  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  async onApplicationBootstrap(): Promise<void> {
    // 6-field cron: `sec min hour dom month dow`. Fires every 10 seconds here
    // so the example is quick to observe — use `"0 0 9 * * *"` for 09:00 daily.
    await this.queue.addSchedule(
      "daily-report",
      "*/10 * * * * *",
      "report.generate",
      { scope: "daily" },
    );
    this.logger.log("registered schedule: daily-report (every 10s for demo)");
  }
}
