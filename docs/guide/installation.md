# Installation

## Requirements

- Node.js **20** or newer
- PostgreSQL **14** or newer
- pnpm or yarn (npm works but has [rough edges with NAPI-RS](https://napi.rs/docs/introduction/getting-started#why-not-npm))

## Install the package

::: code-group

```bash [pnpm]
pnpm add @eddyq/queue
```

```bash [yarn]
yarn add @eddyq/queue
```

```bash [npm]
npm install @eddyq/queue
```

:::

## NestJS

If you're on NestJS, install the module package alongside the core client:

```bash
pnpm add @eddyq/queue @eddyq/nestjs
```

See the [NestJS guide](/nestjs/setup) for `forRoot` configuration and decorator usage.

## Database setup

eddyq owns its own schema. You don't create tables manually — but you **do** need to apply migrations explicitly before starting workers.

```bash
npx eddyq migrate run --database-url "$DATABASE_URL"
```

See [Migrations](./migrations) for the full rationale and the Node-script alternative.
