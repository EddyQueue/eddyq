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
