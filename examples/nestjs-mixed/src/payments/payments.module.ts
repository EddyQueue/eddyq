import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { PaymentsController } from "./payments.controller.js";
import { PaymentsProcessor } from "./payments.processor.js";

@Module({
  imports: [
    EddyqModule.registerQueue({
      name: "payments",
      defaults: { maxAttempts: 8 },
    }),
  ],
  controllers: [PaymentsController],
  providers: [PaymentsProcessor],
})
export class PaymentsModule {}
