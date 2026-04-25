import { Module } from "@nestjs/common";

import { ReportsProcessor } from "./reports.processor.js";

@Module({
  providers: [ReportsProcessor],
})
export class ReportsModule {}
