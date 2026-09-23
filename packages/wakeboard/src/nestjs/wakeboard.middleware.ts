import { Inject, Injectable, type NestMiddleware } from '@nestjs/common';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { WAKEBOARD_OPTIONS } from './wakeboard.constants.js';
import type { EddyqWakeboardOptions } from './wakeboard.types.js';

@Injectable()
export class WakeboardAuthMiddleware implements NestMiddleware {
  private readonly expected: string;
  private readonly configured: boolean;

  constructor(@Inject(WAKEBOARD_OPTIONS) opts: EddyqWakeboardOptions) {
    const pass = opts.auth?.password;
    const user = opts.auth?.username ?? 'admin';
    this.configured = !!pass;
    this.expected = pass
      ? 'Basic ' + Buffer.from(`${user}:${pass}`).toString('base64')
      : '';
  }

  // Fastify hands Nest middleware the raw Node objects, not Express's
  // req/res, so stick to the `node:http` API that both adapters provide.
  use(req: IncomingMessage, res: ServerResponse, next: () => void) {
    if (!this.configured) {
      reply(res, 503, 'Set auth.password in EddyqWakeboardModule.forRoot()');
      return;
    }
    if (req.headers['authorization'] !== this.expected) {
      res.setHeader('WWW-Authenticate', 'Basic realm="eddyq-wakeboard"');
      reply(res, 401, 'Unauthorized');
      return;
    }
    next();
  }
}

function reply(res: ServerResponse, status: number, body: string): void {
  res.statusCode = status;
  res.setHeader('Content-Type', 'text/plain; charset=utf-8');
  res.end(body);
}
