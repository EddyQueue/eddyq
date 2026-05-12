import {
  Controller,
  type DynamicModule,
  Inject,
  Logger,
  Module,
  type MiddlewareConsumer,
  type NestModule,
  type OnModuleInit,
  RequestMethod,
} from '@nestjs/common';
import { HttpAdapterHost } from '@nestjs/core';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WakeboardAuthMiddleware } from './wakeboard.middleware.js';
import { WAKEBOARD_OPTIONS } from './wakeboard.constants.js';
import { WakeboardControllerBase } from './wakeboard.controller.base.js';
import { WakeboardService } from './wakeboard.service.js';
import type { EddyqWakeboardOptions } from './wakeboard.types.js';

const DIST_PUBLIC = join(dirname(fileURLToPath(import.meta.url)), 'public');

@Module({})
export class EddyqWakeboardModule implements NestModule, OnModuleInit {
  private static readonly logger = new Logger(EddyqWakeboardModule.name);
  // Stored at forRoot() time so configure() and onModuleInit() can read it
  // without DI gymnastics.
  private static _mountPath = '/wakeboard';

  constructor(
    private readonly adapterHost: HttpAdapterHost,
    @Inject(WAKEBOARD_OPTIONS) private readonly options: EddyqWakeboardOptions,
  ) {}

  static forRoot(options: EddyqWakeboardOptions = {}): DynamicModule {
    const mountPath = (options.mountPath ?? '/wakeboard').replace(/\/$/, '');
    EddyqWakeboardModule._mountPath = mountPath;

    // Create a controller subclass with the configured path prefix applied.
    @Controller(mountPath)
    class MountedWakeboardController extends WakeboardControllerBase {}

    return {
      module: EddyqWakeboardModule,
      providers: [
        { provide: WAKEBOARD_OPTIONS, useValue: { ...options, mountPath } },
        WakeboardService,
        WakeboardAuthMiddleware,
      ],
      controllers: [MountedWakeboardController],
    };
  }

  configure(consumer: MiddlewareConsumer) {
    consumer
      .apply(WakeboardAuthMiddleware)
      .forRoutes({ path: `${EddyqWakeboardModule._mountPath}*path`, method: RequestMethod.ALL });
  }

  // Static asset serving is delegated to the underlying HTTP adapter rather
  // than handled by a Nest controller. The Nest controller's catch-all only
  // handles the SPA fallback (any path that doesn't match a built asset).
  //
  // This split mirrors what `@bull-board/fastify` does: the dashboard plugin
  // reaches the underlying Fastify instance via `HttpAdapterHost` and calls
  // `instance.register(@fastify/static, …)` directly. We do the same here
  // for Fastify, and the equivalent `instance.use(express.static(…))` for
  // Express. Doing it this way avoids two problems:
  //
  //   1. `FastifyReply` has no `sendFile` method — using `@fastify/static`
  //      gets us proper MIME types, ETag, and range support natively.
  //   2. Nest controllers route through path-to-regexp on Express and
  //      find-my-way on Fastify; the two have incompatible wildcard syntax,
  //      and a `@Get('*path')`-style decorator that works on Express will
  //      silently fail to register on Fastify.
  async onModuleInit(): Promise<void> {
    const assetsRoot = join(DIST_PUBLIC, 'assets');
    if (!existsSync(assetsRoot)) {
      EddyqWakeboardModule.logger.warn(
        `wakeboard frontend assets not found at ${assetsRoot}; ` +
          `run \`pnpm --filter @eddyq/wakeboard build:frontend\``,
      );
      return;
    }

    const httpAdapter = this.adapterHost.httpAdapter;
    if (!httpAdapter) {
      EddyqWakeboardModule.logger.warn(
        'HttpAdapter not available; skipping static asset registration',
      );
      return;
    }

    const mountPath = EddyqWakeboardModule._mountPath;
    const assetsPrefix = `${mountPath}/assets/`;
    const adapterType = httpAdapter.getType();
    const instance = httpAdapter.getInstance();

    if (adapterType === 'fastify') {
      let fastifyStatic: unknown;
      try {
        fastifyStatic = (await import('@fastify/static')).default;
      } catch (err) {
        throw new Error(
          '@eddyq/wakeboard requires `@fastify/static` when running on the Fastify adapter. ' +
            'Install it: `npm i @fastify/static`.',
          { cause: err as Error },
        );
      }
      await instance.register(fastifyStatic as never, {
        root: assetsRoot,
        prefix: assetsPrefix,
        decorateReply: false,
      });
      EddyqWakeboardModule.logger.log(
        `registered @fastify/static at ${assetsPrefix} → ${assetsRoot}`,
      );
      return;
    }

    if (adapterType === 'express') {
      let express: { static: (root: string) => unknown };
      try {
        express = (await import('express')).default as never;
      } catch (err) {
        throw new Error(
          '@eddyq/wakeboard requires `express` when running on the Express adapter.',
          { cause: err as Error },
        );
      }
      // Drop trailing slash so `instance.use('/wakeboard/assets', …)` matches
      // both `/wakeboard/assets/x.js` and (theoretically) `/wakeboard/assets`.
      instance.use(`${mountPath}/assets`, express.static(assetsRoot));
      EddyqWakeboardModule.logger.log(
        `registered express.static at ${mountPath}/assets → ${assetsRoot}`,
      );
      return;
    }

    EddyqWakeboardModule.logger.warn(
      `unknown HTTP adapter type "${adapterType}"; static assets not served`,
    );
  }
}
