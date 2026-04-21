import { Body, Controller, Post } from "@nestjs/common";

import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

interface SendEmailBody {
  to: string;
  subject: string;
}

interface SendBulkBody {
  messages: SendEmailBody[];
}

@Controller("email")
export class EmailController {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  @Post("send")
  async send(@Body() body: SendEmailBody): Promise<{ jobId: number | undefined }> {
    const r = await this.queue.enqueue("send.email", body, {
      uniqueKey: `email:${body.to}:${Date.now()}`,
    });
    return { jobId: r.id };
  }

  /**
   * Bulk enqueue. One Postgres round-trip for the whole batch — a 500-message
   * payload takes roughly the same time as a 50-message payload. Per-message
   * `uniqueKey` still deduplicates against the existing queue, so resubmitting
   * the same list is safe.
   */
  @Post("send-bulk")
  async sendBulk(
    @Body() body: SendBulkBody,
  ): Promise<{ inserted: number; skipped: number }> {
    const stamp = Date.now();
    const r = await this.queue.enqueueMany(
      body.messages.map((m) => ({
        kind: "send.email",
        payload: m,
        uniqueKey: `email:${m.to}:${stamp}`,
      })),
    );
    return { inserted: Number(r.inserted), skipped: Number(r.skipped) };
  }
}
