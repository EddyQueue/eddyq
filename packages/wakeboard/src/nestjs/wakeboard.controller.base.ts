import { Controller, Get, Inject, Param, Post, Query, Res } from '@nestjs/common';
import type { Response } from 'express';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WAKEBOARD_OPTIONS } from './wakeboard.constants.js';
import { WakeboardService } from './wakeboard.service.js';
import type { EddyqWakeboardOptions } from './wakeboard.types.js';

const DIST_PUBLIC = join(dirname(fileURLToPath(import.meta.url)), 'public');
const VALID_STATES = new Set(['pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled']);
const VALID_PROVIDERS = new Set(['postgres', 'redis']);

function pickProvider(s?: string): 'postgres' | 'redis' | undefined {
  return s && VALID_PROVIDERS.has(s) ? (s as 'postgres' | 'redis') : undefined;
}

@Controller()
export class WakeboardControllerBase {
  private readonly indexHtml: string | null = null;

  constructor(
    protected readonly service: WakeboardService,
    @Inject(WAKEBOARD_OPTIONS) protected readonly opts: EddyqWakeboardOptions,
  ) {
    const mountPath = opts.mountPath ?? '/wakeboard';
    const htmlPath = join(DIST_PUBLIC, 'index.html');
    if (existsSync(htmlPath)) {
      const raw = readFileSync(htmlPath, 'utf-8');
      this.indexHtml = raw
        .replace('__WAKEBOARD_BASE__', `${mountPath}/`)
        .replace("'__EDDYQ_API_BASE__'", `'${mountPath}'`);
    }
  }

  // --- REST API ---

  /** Backends this dashboard can talk to (1 or 2). */
  @Get('api/providers')
  providers() {
    return { providers: this.service.providers() };
  }

  @Get('api/stats')
  stats(@Query('provider') provider?: string) {
    const p = pickProvider(provider);
    return p ? this.service.getStatsFor(p) : this.service.getStats();
  }

  @Get('api/jobs')
  jobs(
    @Query('queue') queue?: string,
    @Query('state') state?: string,
    @Query('kind') kind?: string,
    @Query('groupKey') groupKey?: string,
    @Query('tag') tag?: string,
    @Query('page') page = '1',
    @Query('provider') provider?: string,
  ) {
    const offset = (Math.max(1, parseInt(page, 10) || 1) - 1) * 50;
    const safeState = state && VALID_STATES.has(state) ? state : undefined;
    return this.service.listJobs(
      { queue, state: safeState, kind, groupKey, tag },
      { limit: 50, offset },
      pickProvider(provider),
    );
  }

  @Get('api/queues')
  queues(@Query('provider') provider?: string) {
    return this.service.listQueues(pickProvider(provider));
  }

  @Get('api/groups')
  groups(@Query('provider') provider?: string) {
    return this.service.listGroups(pickProvider(provider));
  }

  @Get('api/schedules')
  schedules(@Query('provider') provider?: string) {
    return this.service.listSchedules(pickProvider(provider));
  }

  @Post('api/jobs/:id/cancel')
  cancelJob(@Param('id') id: string, @Query('provider') provider?: string) {
    return this.service.cancelJob(parseInt(id, 10), pickProvider(provider));
  }

  @Post('api/queues/:name/pause')
  pauseQueue(@Param('name') name: string, @Query('provider') provider?: string) {
    return this.service.pauseQueue(name, pickProvider(provider));
  }

  @Post('api/queues/:name/resume')
  resumeQueue(@Param('name') name: string, @Query('provider') provider?: string) {
    return this.service.resumeQueue(name, pickProvider(provider));
  }

  @Post('api/groups/:key/pause')
  pauseGroup(@Param('key') key: string, @Query('provider') provider?: string) {
    return this.service.pauseGroup(key, pickProvider(provider));
  }

  @Post('api/groups/:key/resume')
  resumeGroup(@Param('key') key: string, @Query('provider') provider?: string) {
    return this.service.resumeGroup(key, pickProvider(provider));
  }

  @Post('api/schedules/:name/enable')
  enableSchedule(@Param('name') name: string, @Query('provider') provider?: string) {
    return this.service.setScheduleEnabled(name, true, pickProvider(provider));
  }

  @Post('api/schedules/:name/disable')
  disableSchedule(@Param('name') name: string, @Query('provider') provider?: string) {
    return this.service.setScheduleEnabled(name, false, pickProvider(provider));
  }

  @Post('api/schedules/:name/remove')
  removeSchedule(@Param('name') name: string, @Query('provider') provider?: string) {
    return this.service.removeSchedule(name, pickProvider(provider));
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
      res.status(503).send('Run "pnpm build:frontend" in packages/wakeboard first.');
    }
  }
}
