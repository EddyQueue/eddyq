import { Injectable } from "@nestjs/common";
import { DiscoveryService } from "@nestjs/core";

import {
  EDDYQ_QUEUE_REGISTRATION_PREFIX,
  getQueueToken,
} from "./eddyq.constants.js";
import type { QueueHandleImpl } from "./eddyq-queue.handle.js";
import type { QueueRegistration } from "./eddyq.types.js";

export interface AggregatedRegistration {
  registration: QueueRegistration;
  handle: QueueHandleImpl;
}

/**
 * Walks the Nest container at bootstrap and collects every provider produced
 * by `EddyqModule.registerQueue`. Each `registerQueue` call emits a value
 * provider under a token of the form `EDDYQ_QUEUE_REGISTRATION:<name>`; the
 * aggregator scans `DiscoveryService.getProviders()` for that prefix and
 * pairs each registration with its matching {@link QueueHandleImpl} provider.
 *
 * Used by the module at `onApplicationBootstrap` to derive group config,
 * schedules, and the worker subscription set.
 */
@Injectable()
export class EddyqQueueAggregator {
  constructor(private readonly discovery: DiscoveryService) {}

  collect(): AggregatedRegistration[] {
    const seen = new Set<string>();
    const out: AggregatedRegistration[] = [];
    const providers = this.discovery.getProviders();

    for (const wrapper of providers) {
      const token = wrapper.name;
      if (
        typeof token !== "string" ||
        !token.startsWith(EDDYQ_QUEUE_REGISTRATION_PREFIX)
      ) {
        continue;
      }
      const reg = wrapper.instance as QueueRegistration | undefined;
      if (!reg || typeof reg.name !== "string") continue;

      if (seen.has(reg.name)) {
        throw new Error(
          `@eddyq/nestjs: duplicate registerQueue for "${reg.name}". ` +
            `Each queue name may be registered only once per app.`,
        );
      }
      seen.add(reg.name);

      const handleToken = getQueueToken(reg.name);
      const handleWrapper = providers.find((w) => w.name === handleToken);
      if (!handleWrapper || !handleWrapper.instance) {
        throw new Error(
          `@eddyq/nestjs: queue handle for "${reg.name}" was not instantiated. ` +
            `This is a bug — please report it.`,
        );
      }
      out.push({
        registration: reg,
        handle: handleWrapper.instance as QueueHandleImpl,
      });
    }

    return out;
  }
}
