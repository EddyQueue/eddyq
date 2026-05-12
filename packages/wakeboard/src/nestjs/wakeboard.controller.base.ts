import { Controller, Get, Inject, Param, Post, Query, Res } from '@nestjs/common';
import type { Response } from 'express';
import { existsSync, readFileSync } from 'node:fs';
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
      this.indexHtml = raw.replace('__WAKEBOARD_BASE__', `${mountPath}/`);
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

  // --- SPA fallback (catch-all, must be last) ---
  //
  // Static assets are served outside this controller — see
  // `EddyqWakeboardModule.onModuleInit`, which registers `@fastify/static`
  // or `express.static` on the underlying HTTP adapter for the
  // `${mountPath}/assets/` prefix. Anything that reaches *this* handler
  // is either the dashboard root or a client-side Svelte route, so we
  // always respond with `index.html`.
  //
  // The wildcard is plain `*` rather than a named param (`*path`) so the
  // route registers on both adapters. The Express adapter routes through
  // path-to-regexp v8 which auto-converts `*` via Nest's compatibility
  // shim; the Fastify adapter routes through find-my-way which only
  // understands bare `*` as a wildcard. Named-wildcard syntax (`*path`)
  // breaks Fastify route registration silently.

  @Get()
  serveRoot(@Res() res: Response) {
    return this.sendIndex(res);
  }

  @Get('*')
  serveSpa(@Res() res: Response) {
    return this.sendIndex(res);
  }

  private sendIndex(res: Response) {
    if (this.indexHtml) {
      // Use the full MIME type string, not the `html` shortcut: Fastify's
      // `reply.type()` is an alias for `header('Content-Type', value)` and
      // does no shortcut resolution, so `type('html')` sets a literal
      // `content-type: html` header. Combined with `X-Content-Type-Options:
      // nosniff` (set by helmet et al.) the browser renders the response
      // as plain text instead of HTML.
      res.type('text/html; charset=utf-8').send(this.indexHtml);
    } else {
      res.status(503).send('Run "pnpm build:frontend" in packages/wakeboard first.');
    }
  }
}

// Hide wakeboard routes from any `@nestjs/swagger`-generated OpenAPI doc the
// host app builds. The SPA catch-all (`@Get('*')`) and `:param` paths trip up
// codegen tools like orval that consume the spec, and there is no useful
// schema to publish for an admin UI mount. Setting the metadata key directly
// avoids adding `@nestjs/swagger` as a dependency — if Swagger is not in use,
// this key is simply ignored.
Reflect.defineMetadata(
  'swagger/apiExcludeController',
  [{ disable: true }],
  WakeboardControllerBase,
);
