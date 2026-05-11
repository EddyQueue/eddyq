import { Logger } from "@nestjs/common";

import { JobHandler, Processor, type JobCall } from "@eddyq/nestjs";

@Processor()
export class WebhooksProcessor {
  private readonly logger = new Logger(WebhooksProcessor.name);

  @JobHandler("webhook.deliver")
  async deliver({ payload, id }: JobCall): Promise<{ url: string; status: number }> {
    const { url } = payload as { url: string };
    this.logger.log(`webhook.deliver #${id} → ${url}`);
    // Pretend we POSTed somewhere.
    return { url, status: 200 };
  }
}
