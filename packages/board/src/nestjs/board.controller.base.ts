import { Controller, Get, Inject, Param, Post, Query, Res } from '@nestjs/common';
import type { Response } from 'express';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { BOARD_OPTIONS } from './board.constants.js';
import { BoardService } from './board.service.js';
import type { EddyqBoardOptions } from './board.types.js';

const DIST_PUBLIC = join(dirname(fileURLToPath(import.meta.url)), 'public');
const VALID_STATES = new Set(['pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled']);

@Controller()
export class BoardControllerBase {
  private readonly indexHtml: string | null = null;

  constructor(
    protected readonly service: BoardService,
    @Inject(BOARD_OPTIONS) protected readonly opts: EddyqBoardOptions,
  ) {
    const mountPath = opts.mountPath ?? '/board';
    const htmlPath = join(DIST_PUBLIC, 'index.html');
    if (existsSync(htmlPath)) {
      const raw = readFileSync(htmlPath, 'utf-8');
      this.indexHtml = raw
        .replace('__BOARD_BASE__', `${mountPath}/`)
        .replace("'__EDDYQ_API_BASE__'", `'${mountPath}'`);
    }
  }

  // --- REST API ---

  @Get('api/stats')
  stats() {
    return this.service.getStats();
  }

  @Get('api/jobs')
  jobs(
    @Query('queue') queue?: string,
    @Query('state') state?: string,
    @Query('kind') kind?: string,
    @Query('groupKey') groupKey?: string,
    @Query('tag') tag?: string,
    @Query('page') page = '1',
  ) {
    const offset = (Math.max(1, parseInt(page, 10) || 1) - 1) * 50;
    const safeState = state && VALID_STATES.has(state) ? state : undefined;
    return this.service.listJobs({ queue, state: safeState, kind, groupKey, tag }, { limit: 50, offset });
  }

  @Get('api/queues')
  queues() {
    return this.service.listQueues();
  }

  @Get('api/groups')
  groups() {
    return this.service.listGroups();
  }

  @Get('api/schedules')
  schedules() {
    return this.service.listSchedules();
  }

  @Post('api/jobs/:id/cancel')
  cancelJob(@Param('id') id: string) {
    return this.service.cancelJob(parseInt(id, 10));
  }

  @Post('api/queues/:name/pause')
  pauseQueue(@Param('name') name: string) {
    return this.service.pauseQueue(name);
  }

  @Post('api/queues/:name/resume')
  resumeQueue(@Param('name') name: string) {
    return this.service.resumeQueue(name);
  }

  @Post('api/groups/:key/pause')
  pauseGroup(@Param('key') key: string) {
    return this.service.pauseGroup(key);
  }

  @Post('api/groups/:key/resume')
  resumeGroup(@Param('key') key: string) {
    return this.service.resumeGroup(key);
  }

  @Post('api/schedules/:name/enable')
  enableSchedule(@Param('name') name: string) {
    return this.service.setScheduleEnabled(name, true);
  }

  @Post('api/schedules/:name/disable')
  disableSchedule(@Param('name') name: string) {
    return this.service.setScheduleEnabled(name, false);
  }

  @Post('api/schedules/:name/remove')
  removeSchedule(@Param('name') name: string) {
    return this.service.removeSchedule(name);
  }

  // --- SPA serving (catch-all, must be last) ---

  @Get()
  serveRoot(@Res() res: Response) {
    return this.serveStatic('', res);
  }

  @Get('*path')
  serveStatic(@Param('path') path: string | string[], @Res() res: Response) {
    // Serve built assets; fall back to index.html for client-side routes.
    const resolved = Array.isArray(path) ? path.join('/') : path;
    const filePath = join(DIST_PUBLIC, resolved);
    if (resolved && existsSync(filePath) && statSync(filePath).isFile()) {
      res.sendFile(filePath);
      return;
    }
    if (this.indexHtml) {
      res.type('html').send(this.indexHtml);
    } else {
      res.status(503).send('Run "pnpm build:frontend" in packages/board first.');
    }
  }
}
