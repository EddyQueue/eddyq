import "reflect-metadata";

import { Logger } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";

import { WorkersModule } from "./workers.module.js";

/**
 * Worker entry. `createApplicationContext` boots the DI container without
 * an HTTP listener — that's what a worker pod needs.
 */
async function bootstrap(): Promise<void> {
  const app = await NestFactory.createApplicationContext(WorkersModule, {
    logger: ["log", "warn", "error"],
  });
  app.enableShutdownHooks();
  Logger.log("worker runtime started", "Bootstrap");
}

void bootstrap();
