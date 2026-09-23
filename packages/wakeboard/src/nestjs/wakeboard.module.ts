import {
  Controller,
  type DynamicModule,
  Inject,
  Logger,
  Module,
  type MiddlewareConsumer,
  type NestModule,
  type OnModuleInit,
  type Type,
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
  private static _controller: Type<WakeboardControllerBase>;

  constructor(
    private readonly adapterHost: HttpAdapterHost,
    @Inject(WAKEBOARD_OPTIONS) private readonly options: EddyqWakeboardOptions,
    private readonly auth: WakeboardAuthMiddleware,
  ) {}

  static forRoot(options: EddyqWakeboardOptions = {}): DynamicModule {
    const mountPath = (options.mountPath ?? '/wakeboard').replace(/\/$/, '');
    EddyqWakeboardModule._mountPath = mountPath;

    // Create a controller subclass with the configured path prefix applied.
    @Controller(mountPath)
    class MountedWakeboardController extends WakeboardControllerBase {}
    EddyqWakeboardModule._controller = MountedWakeboardController;

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

  async configure(consumer: MiddlewareConsumer): Promise<void> {
    // Bind to the controller rather than a wildcard path: wildcard syntax
    // differs between Nest 10 (`(.*)`) and 11+ (`*path`), and a path the
    // router doesn't understand leaves every route unauthenticated.
    consumer.apply(WakeboardAuthMiddleware).forRoutes(EddyqWakeboardModule._controller);

    // Express runs handlers in registration order and Nest registers
    // controller routes after `configure()` but before `onModuleInit()`.
    // Registered any later, `express.static` would sit behind the SPA
    // catch-all and every asset request would get `index.html`.
    if (this.adapterHost.httpAdapter?.getType() === 'express') {
      await this.registerExpressAssets();
    }
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
    const httpAdapter = this.adapterHost.httpAdapter;
    if (!httpAdapter) {
      EddyqWakeboardModule.logger.warn(
        'HttpAdapter not available; skipping static asset registration',
      );
      return;
    }

    const adapterType = httpAdapter.getType();
    // Express assets were registered in `configure()`.
    if (adapterType === 'express') return;
    if (adapterType !== 'fastify') {
      EddyqWakeboardModule.logger.warn(
        `unknown HTTP adapter type "${adapterType}"; static assets not served`,
      );
      return;
    }

    const assetsRoot = this.assetsRootOrWarn();
    if (!assetsRoot) return;

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
    // Fastify routes are matched by find-my-way, not registration order, so
    // registering here (after the controller routes) is fine.
    const assetsPrefix = `${EddyqWakeboardModule._mountPath}/assets/`;
    await httpAdapter.getInstance().register(fastifyStatic as never, {
      root: assetsRoot,
      prefix: assetsPrefix,
      decorateReply: false,
    });
    EddyqWakeboardModule.logger.log(
      `registered @fastify/static at ${assetsPrefix} → ${assetsRoot}`,
    );
  }

  private async registerExpressAssets(): Promise<void> {
    const assetsRoot = this.assetsRootOrWarn();
    if (!assetsRoot) return;

    let express: { static: (root: string) => unknown };
    try {
      express = (await import('express')).default as never;
    } catch (err) {
      throw new Error(
        '@eddyq/wakeboard requires `express` when running on the Express adapter.',
        { cause: err as Error },
      );
    }
    const mountPath = EddyqWakeboardModule._mountPath;
    // Registered ahead of Nest's own middleware, so apply auth explicitly.
    // Drop trailing slash so `instance.use('/wakeboard/assets', …)` matches
    // both `/wakeboard/assets/x.js` and (theoretically) `/wakeboard/assets`.
    this.adapterHost.httpAdapter.getInstance().use(
      `${mountPath}/assets`,
      this.auth.use.bind(this.auth),
      express.static(assetsRoot),
    );
    EddyqWakeboardModule.logger.log(
      `registered express.static at ${mountPath}/assets → ${assetsRoot}`,
    );
  }

  private assetsRootOrWarn(): string | undefined {
    const assetsRoot = join(DIST_PUBLIC, 'assets');
    if (existsSync(assetsRoot)) return assetsRoot;
    EddyqWakeboardModule.logger.warn(
      `wakeboard frontend assets not found at ${assetsRoot}; ` +
        `run \`pnpm --filter @eddyq/wakeboard build:frontend\``,
    );
    return undefined;
  }
}
