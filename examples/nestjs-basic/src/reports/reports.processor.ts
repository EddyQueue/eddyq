import { Logger } from "@nestjs/common";

import { JobHandler, Processor, type JobCall } from "@eddyq/nestjs";

interface BatchEnvelope {
  batchId: number;
  total: number;
  completed: number;
  failed: number;
  cancelled: number;
  durationMs: number;
}

@Processor()
export class ReportsProcessor {
  private readonly logger = new Logger(ReportsProcessor.name);

  @JobHandler("report.generate")
  async generate({ payload, id }: JobCall): Promise<void> {
    this.logger.log(`report.generate #${id}: ${JSON.stringify(payload)}`);
    // In a real app: build the report, write it somewhere, notify stakeholders.
  }

  @JobHandler("report.shard")
  async shard({ payload, id }: JobCall): Promise<void> {
    const { scope, shard } = payload as { scope: string; shard: number };
    this.logger.log(`report.shard #${id}: scope=${scope} shard=${shard}`);
    // In a real app: process this slice of the report.
  }

  /**
   * Fires once per batch run — see `reports.controller.ts`. The `_eddyq_batch`
   * envelope is injected by eddyq alongside the user payload; everything else
   * in `payload` is whatever you set on `onComplete`.
   */
  @JobHandler("report.summary")
  async summary({ payload, id }: JobCall): Promise<void> {
    const { _eddyq_batch, ...user } = payload as {
      _eddyq_batch: BatchEnvelope;
      [k: string]: unknown;
    };
    if (_eddyq_batch.failed > 0 || _eddyq_batch.cancelled > 0) {
      this.logger.warn(
        `report.summary #${id}: scope=${(user as { scope?: string }).scope} ` +
          `${_eddyq_batch.completed}/${_eddyq_batch.total} ok ` +
          `(${_eddyq_batch.failed} failed, ${_eddyq_batch.cancelled} cancelled) ` +
          `in ${_eddyq_batch.durationMs}ms`,
      );
      return;
    }
    this.logger.log(
      `report.summary #${id}: scope=${(user as { scope?: string }).scope} ` +
        `all ${_eddyq_batch.total} shards complete in ${_eddyq_batch.durationMs}ms`,
    );
  }
}
