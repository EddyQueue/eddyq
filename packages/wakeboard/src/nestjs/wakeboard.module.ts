import { Controller, type DynamicModule, Module, type MiddlewareConsumer, type NestModule, RequestMethod } from '@nestjs/common';
import { WakeboardAuthMiddleware } from './wakeboard.middleware.js';
import { WAKEBOARD_OPTIONS } from './wakeboard.constants.js';
import { WakeboardControllerBase } from './wakeboard.controller.base.js';
import { WakeboardService } from './wakeboard.service.js';
import type { EddyqWakeboardOptions } from './wakeboard.types.js';

@Module({})
export class EddyqWakeboardModule implements NestModule {
  // Stored at forRoot() time so configure() can read it without DI gymnastics.
  private static _mountPath = '/wakeboard';

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
}
