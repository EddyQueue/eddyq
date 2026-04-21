import { Logger } from "@nestjs/common";

import { JobHandler, Processor, type JobCall } from "@eddyq/nestjs";

@Processor()
export class ReportsProcessor {
  private readonly logger = new Logger(ReportsProcessor.name);

  @JobHandler("report.generate")
  async generate({ payload, id }: JobCall): Promise<void> {
    this.logger.log(`report.generate #${id}: ${JSON.stringify(payload)}`);
    // In a real app: build the report, write it somewhere, notify stakeholders.
  }
}
