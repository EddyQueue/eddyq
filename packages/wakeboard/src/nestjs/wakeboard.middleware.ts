import { Inject, Injectable, type NestMiddleware } from '@nestjs/common';
import type { NextFunction, Request, Response } from 'express';
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

  use(req: Request, res: Response, next: NextFunction) {
    if (!this.configured) {
      res.status(503).send('Set auth.password in EddyqWakeboardModule.forRoot()');
      return;
    }
    const auth = (req.headers['authorization'] as string) ?? '';
    if (auth !== this.expected) {
      res.setHeader('WWW-Authenticate', 'Basic realm="eddyq-wakeboard"');
      res.status(401).send('Unauthorized');
      return;
    }
    next();
  }
}
