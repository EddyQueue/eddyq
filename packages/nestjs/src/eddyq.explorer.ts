import { Injectable, Logger } from "@nestjs/common";
import { DiscoveryService, MetadataScanner, Reflector } from "@nestjs/core";

import {
  EDDYQ_JOB_HANDLER_META,
  EDDYQ_PROCESSOR_META,
} from "./eddyq.constants.js";
import type { JobHandlerFn } from "./eddyq.types.js";

export interface DiscoveredHandler {
  kind: string;
  /** Bound method — ready to pass to `eddyq.work(kind, fn)`. */
  handler: JobHandlerFn;
  /** For log context (e.g., "EmailProcessor.send"). */
  label: string;
}

/**
 * Walks the Nest container at bootstrap, collects every method annotated with
 * `@JobHandler(kind)` on a `@Processor()` class, and returns them bound to
 * their owning provider instance.
 */
@Injectable()
export class EddyqExplorer {
  private readonly logger = new Logger(EddyqExplorer.name);

  constructor(
    private readonly discovery: DiscoveryService,
    private readonly metadataScanner: MetadataScanner,
    private readonly reflector: Reflector,
  ) {}

  discover(): DiscoveredHandler[] {
    const providers = this.discovery.getProviders();
    const handlers: DiscoveredHandler[] = [];
    const seen = new Set<string>();

    for (const wrapper of providers) {
      const { instance } = wrapper;
      if (!instance || typeof instance !== "object") continue;

      const ctor = instance.constructor;
      if (!ctor) continue;

      const isProcessor = this.reflector.get<boolean>(
        EDDYQ_PROCESSOR_META,
        ctor,
      );
      if (!isProcessor) continue;

      const prototype = Object.getPrototypeOf(instance) as object | null;
      if (!prototype) continue;

      for (const methodName of this.metadataScanner.getAllMethodNames(
        prototype,
      )) {
        const kind = this.reflector.get<string | undefined>(
          EDDYQ_JOB_HANDLER_META,
          (instance as Record<string, unknown>)[methodName] as Function,
        );
        if (!kind) continue;

        if (seen.has(kind)) {
          throw new Error(
            `@eddyq/nestjs: duplicate @JobHandler for kind ${JSON.stringify(
              kind,
            )} — only one processor method may handle a given kind.`,
          );
        }
        seen.add(kind);

        const fn = (instance as Record<string, Function>)[methodName].bind(
          instance,
        ) as JobHandlerFn;

        handlers.push({
          kind,
          handler: fn,
          label: `${ctor.name}.${methodName}`,
        });
      }
    }

    if (handlers.length === 0) {
      this.logger.warn(
        "No @JobHandler methods found. Workers won't process any kinds.",
      );
    } else {
      for (const h of handlers) {
        this.logger.log(`registered ${h.label} → "${h.kind}"`);
      }
    }

    return handlers;
  }
}
