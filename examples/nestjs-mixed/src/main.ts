import "reflect-metadata";

import { Logger } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";

import { AppModule } from "./app.module.js";

async function bootstrap(): Promise<void> {
  const app = await NestFactory.create(AppModule, {
    logger: ["log", "warn", "error"],
  });
  app.enableShutdownHooks();
  const port = Number(process.env.PORT ?? 3000);
  await app.listen(port);
  Logger.log(
    `mixed-backend app listening on http://localhost:${port}`,
    "Bootstrap",
  );
  Logger.log(
    "POST /webhooks/fire to enqueue on Redis · POST /payments/charge to enqueue on Postgres",
    "Bootstrap",
  );
}

void bootstrap();
