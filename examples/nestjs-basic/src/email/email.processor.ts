import { Logger } from "@nestjs/common";

import { JobHandler, Processor, type JobCall } from "@eddyq/nestjs";

@Processor()
export class EmailProcessor {
  private readonly logger = new Logger(EmailProcessor.name);

  @JobHandler("send.email")
  async send({ payload, id }: JobCall): Promise<void> {
    const { to, subject } = payload as { to: string; subject: string };
    this.logger.log(`send.email #${id}: to=${to} subject="${subject}"`);
    // In a real app: await sendgrid.send({ to, subject, ... })
    // Throw any Error to retry with exponential backoff.
  }
}
