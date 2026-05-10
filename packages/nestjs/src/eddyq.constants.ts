// DI tokens + reflection metadata keys for @eddyq/nestjs.
//
// String tokens (not Symbols) so that users who inject by token name in tests
// or custom providers can type them directly.

/**
 * DI token holding the resolved {@link EddyqModuleOptions} passed to
 * `EddyqModule.forRoot` / `forRootAsync`. Inject when a service needs to
 * read the effective module config — e.g. to derive a related connection
 * URL or to log the queue name at startup.
 *
 * Most consumers should prefer `@InjectEddyq()` to get the queue itself.
 *
 * @example
 * ```ts
 * @Injectable()
 * export class MyService {
 *   constructor(
 *     @Inject(EDDYQ_OPTIONS) private readonly opts: EddyqModuleOptions,
 *   ) {}
 * }
 * ```
 */
export const EDDYQ_OPTIONS = "EDDYQ_OPTIONS";

/**
 * DI token for the live `Eddyq` client instance. Equivalent to using
 * `@InjectEddyq()` — reach for this form when you can't use a parameter
 * decorator, e.g. inside a custom `useFactory` provider.
 *
 * @example
 * ```ts
 * {
 *   provide: 'MY_QUEUE_WRAPPER',
 *   useFactory: (eddyq: Eddyq) => new Wrapper(eddyq),
 *   inject: [EDDYQ_INSTANCE],
 * }
 * ```
 */
export const EDDYQ_INSTANCE = "EDDYQ_INSTANCE";

export const EDDYQ_PROCESSOR_META = "eddyq:processor";
export const EDDYQ_JOB_HANDLER_META = "eddyq:job_handler";

/**
 * Token-prefix for {@link QueueRegistration} value providers emitted by
 * `EddyqModule.registerQueue`. The aggregator at bootstrap iterates every
 * provider whose token starts with this prefix to collect per-queue config.
 *
 * Use {@link getQueueRegistrationToken} to build a concrete token; the prefix
 * is exported only so the aggregator can scan for it.
 */
export const EDDYQ_QUEUE_REGISTRATION_PREFIX = "EDDYQ_QUEUE_REGISTRATION:";

/** Build the token under which a queue registration is provided. */
export const getQueueRegistrationToken = (name: string): string =>
  `${EDDYQ_QUEUE_REGISTRATION_PREFIX}${name}`;

/**
 * DI token for a per-queue {@link QueueHandle}. Use {@link InjectQueue}
 * (e.g. `@InjectQueue('klaviyo')`) instead of constructing the token name
 * manually.
 */
export const getQueueToken = (name: string): string => `EDDYQ_QUEUE:${name}`;
