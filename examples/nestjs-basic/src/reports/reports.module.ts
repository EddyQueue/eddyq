import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { ReportsController } from "./reports.controller.js";
import { ReportsProcessor } from "./reports.processor.js";

@Module({
  imports: [
    EddyqModule.registerQueue({
      name: "reports",
      defaults: { priority: 5 },
      schedules: [
        {
          // every day at 08:00:00 UTC (sec min hour dom month dow)
          name: "daily-report",
          cronExpr: "0 0 8 * * *",
          kind: "report.generate",
          payload: { scope: "daily" },
          priority: 5,
          // queue defaults to the enclosing registerQueue's name ("reports")
        },
      ],
    }),
  ],
  controllers: [ReportsController],
  providers: [ReportsProcessor],
})
export class ReportsModule {}
