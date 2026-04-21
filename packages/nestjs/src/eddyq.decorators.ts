import { Inject, Injectable, SetMetadata } from "@nestjs/common";

import {
  EDDYQ_INSTANCE,
  EDDYQ_JOB_HANDLER_META,
  EDDYQ_PROCESSOR_META,
} from "./eddyq.constants.js";

/**
 * Inject the `Eddyq` queue client into a provider.
 *
 * ```ts
 * constructor(@InjectEddyq() private readonly queue: Eddyq) {}
 * ```
 */
export const InjectEddyq = (): ReturnType<typeof Inject> =>
  Inject(EDDYQ_INSTANCE);

/**
 * Mark a class as an eddyq job processor. The class is registered as an
 * `@Injectable()` provider; at bootstrap, the module scans its methods for
 * `@JobHandler(kind)` annotations and wires each one to `queue.work(kind, …)`.
 *
 * ```ts
 * @Processor()
 * class EmailProcessor {
 *   @JobHandler("send.email")
 *   async send({ payload }: JobCall) { … }
 * }
 * ```
 */
export const Processor = (): ClassDecorator => (target) => {
  Injectable()(target);
  SetMetadata(EDDYQ_PROCESSOR_META, true)(target);
};

/**
 * Register a method as the handler for job kind `kind`. Must live on a class
 * decorated with `@Processor()`.
 */
export const JobHandler = (kind: string): MethodDecorator =>
  SetMetadata(EDDYQ_JOB_HANDLER_META, kind);
