import { Module } from "@nestjs/common";

import { ReportsController } from "./reports.controller.js";
import { ReportsProcessor } from "./reports.processor.js";

@Module({
  controllers: [ReportsController],
  providers: [ReportsProcessor],
})
export class ReportsModule {}
