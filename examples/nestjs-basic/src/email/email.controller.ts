import { Body, Controller, Post } from "@nestjs/common";

import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

interface SendEmailBody {
  to: string;
  subject: string;
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
}
