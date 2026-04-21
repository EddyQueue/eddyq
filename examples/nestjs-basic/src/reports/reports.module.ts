import { Module } from "@nestjs/common";

import { ReportsProcessor } from "./reports.processor.js";
import { ReportsService } from "./reports.service.js";

@Module({
  providers: [ReportsService, ReportsProcessor],
})
export class ReportsModule {}
