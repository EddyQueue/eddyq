import { Module } from "@nestjs/common";

import { EddyqModule } from "@eddyq/nestjs";

import { EmailController } from "./email.controller.js";
import { EmailProcessor } from "./email.processor.js";

@Module({
  imports: [
    EddyqModule.registerQueue({
      name: "email",
      defaults: { maxAttempts: 5 },
    }),
  ],
  controllers: [EmailController],
  providers: [EmailProcessor],
})
export class EmailModule {}
