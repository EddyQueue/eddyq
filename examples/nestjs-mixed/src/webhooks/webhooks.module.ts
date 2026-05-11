import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { WebhooksController } from "./webhooks.controller.js";
import { WebhooksProcessor } from "./webhooks.processor.js";

@Module({
  imports: [
    EddyqModule.registerQueue({
      name: "webhooks",
      defaults: { maxAttempts: 5 },
    }),
  ],
  controllers: [WebhooksController],
  providers: [WebhooksProcessor],
})
export class WebhooksModule {}
