import { Module } from "@nestjs/common";

import { EmailController } from "./email.controller.js";
import { EmailProcessor } from "./email.processor.js";

@Module({
  controllers: [EmailController],
  providers: [EmailProcessor],
})
export class EmailModule {}
