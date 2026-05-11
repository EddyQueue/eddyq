import { Logger } from "@nestjs/common";

import { JobHandler, Processor, type JobCall } from "@eddyq/nestjs";

@Processor()
export class PaymentsProcessor {
  private readonly logger = new Logger(PaymentsProcessor.name);

  @JobHandler("payment.charge")
  async charge({ payload, id }: JobCall): Promise<{ chargedCents: number }> {
    const { customerId, amountCents } = payload as {
      customerId: string;
      amountCents: number;
    };
    this.logger.log(
      `payment.charge #${id} customer=${customerId} amount=${amountCents}`,
    );
    // Real impl: call Stripe inside a DB transaction so the charge record
    // and the queue entry are consistent.
    return { chargedCents: amountCents };
  }
}
