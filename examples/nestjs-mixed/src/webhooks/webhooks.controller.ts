import { Body, Controller, Post } from "@nestjs/common";

import { InjectQueue, type QueueHandle } from "@eddyq/nestjs";

interface FireBody {
  url: string;
  payload?: unknown;
}

@Controller("webhooks")
export class WebhooksController {
  constructor(@InjectQueue("webhooks") private readonly queue: QueueHandle) {}

  @Post("fire")
  async fire(@Body() body: FireBody): Promise<{ id: number | undefined }> {
    // `webhooks` is routed to Redis by `forRoot.queues` — this enqueue
    // lands on the Redis backend, not Postgres.
    const r = await this.queue.enqueue("webhook.deliver", {
      url: body.url,
      payload: body.payload ?? null,
    });
    return { id: r.id };
  }
}
