import { Body, Controller, Post } from "@nestjs/common";

import { InjectQueue, type QueueHandle } from "@eddyq/nestjs";

interface ChargeBody {
  customerId: string;
  amountCents: number;
}

@Controller("payments")
export class PaymentsController {
  constructor(@InjectQueue("payments") private readonly queue: QueueHandle) {}

  @Post("charge")
  async charge(@Body() body: ChargeBody): Promise<{ id: number | undefined }> {
    // `payments` is routed to Postgres — this enqueue lands on PG.
    // If a real handler wraps the charge in a DB transaction, it can
    // `enqueueInTx` from the @InjectQueue handle (Postgres-only escape
    // hatch — would throw at runtime if `payments` were routed to Redis).
    const r = await this.queue.enqueue("payment.charge", body, {
      uniqueKey: `charge:${body.customerId}:${Date.now()}`,
    });
    return { id: r.id };
  }
}
