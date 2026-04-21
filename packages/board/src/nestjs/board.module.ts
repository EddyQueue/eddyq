import { Controller, type DynamicModule, Module, type MiddlewareConsumer, type NestModule, RequestMethod } from '@nestjs/common';
import { BoardAuthMiddleware } from './board.middleware.js';
import { BOARD_OPTIONS } from './board.constants.js';
import { BoardControllerBase } from './board.controller.base.js';
import { BoardService } from './board.service.js';
import type { EddyqBoardOptions } from './board.types.js';

@Module({})
export class EddyqBoardModule implements NestModule {
  // Stored at forRoot() time so configure() can read it without DI gymnastics.
  private static _mountPath = '/board';

  static forRoot(options: EddyqBoardOptions = {}): DynamicModule {
    const mountPath = (options.mountPath ?? '/board').replace(/\/$/, '');
    EddyqBoardModule._mountPath = mountPath;

    // Create a controller subclass with the configured path prefix applied.
    @Controller(mountPath)
    class MountedBoardController extends BoardControllerBase {}

    return {
      module: EddyqBoardModule,
      providers: [
        { provide: BOARD_OPTIONS, useValue: { ...options, mountPath } },
        BoardService,
        BoardAuthMiddleware,
      ],
      controllers: [MountedBoardController],
    };
  }

  configure(consumer: MiddlewareConsumer) {
    consumer
      .apply(BoardAuthMiddleware)
      .forRoutes({ path: `${EddyqBoardModule._mountPath}*`, method: RequestMethod.ALL });
  }
}
