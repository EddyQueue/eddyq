# NestJS setup

```bash
pnpm add @eddyq/wakeboard
```

## Mount it

```ts
import { Module } from '@nestjs/common'
import { EddyqModule } from '@eddyq/nestjs'
import { EddyqWakeboardModule } from '@eddyq/wakeboard'

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.DATABASE_URL!,
    }),
    EddyqWakeboardModule.forRoot({
      mountPath: '/wakeboard',
      auth: { password: process.env.WAKEBOARD_PASSWORD! },
    }),
  ],
})
export class AppModule {}
```

Visit `http://localhost:3000/wakeboard` (or wherever your Nest app listens) and log in with username `admin` and the password you set.

## Options

```ts
interface EddyqWakeboardOptions {
  /** URL path to mount the wakeboard at. Default: '/wakeboard' */
  mountPath?: string

  auth?: {
    /** Default: 'admin' */
    username?: string
    password: string
  }
}
```

If you omit `auth.password` (or omit `auth` entirely), the dashboard returns `503` for every request. This is intentional — there is no "open by default" mode.

## Mount path

Default is `/wakeboard`. Change it if you have a route conflict or want to tuck it behind a less obvious URL:

```ts
EddyqWakeboardModule.forRoot({
  mountPath: '/admin/queue',
  auth: { password: process.env.WAKEBOARD_PASSWORD! },
})
```

## Using your own auth instead

Skip the built-in Basic auth and put a Nest guard in front of the mount path:

```ts
EddyqWakeboardModule.forRoot({
  mountPath: '/wakeboard',
  // no auth field — module's middleware is inert
})

// Apply your existing guard at the route level
@UseGuards(YourAdminGuard)
@Controller('wakeboard')
class GuardedRoot {}
```

This is the right pattern if you already have SSO, role-based access, or session cookies — Wakeboard's Basic auth is just the simple default.

## Async configuration

```ts
EddyqWakeboardModule.forRootAsync?.({
  inject: [ConfigService],
  useFactory: (config: ConfigService) => ({
    mountPath: config.get('WAKEBOARD_PATH', '/wakeboard'),
    auth: { password: config.getOrThrow('WAKEBOARD_PASSWORD') },
  }),
})
```

::: tip In the API process, not the worker
Mount Wakeboard in your **API** composition root, not your worker root — workers don't run an HTTP listener (`createApplicationContext`), so there's nothing to serve from. See the [example app](/nestjs/example#api-composition-root).
:::
