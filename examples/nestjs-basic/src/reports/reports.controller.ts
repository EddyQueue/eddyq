import { Body, Controller, Post } from "@nestjs/common";

import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

interface RunShardsBody {
  scope: string;
  shards: number;
}

@Controller("reports")
export class ReportsController {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  /**
   * Fan-in pattern. Enqueue N shard jobs as one batch; eddyq fires
   * `report.summary` exactly once when every shard reaches a terminal state
   * (completed / failed / cancelled). The summary handler receives counts
   * under `_eddyq_batch` in its payload — branch on those if you want to
   * treat partial failures differently from a clean run.
   */
  @Post("run-shards")
  async runShards(
    @Body() body: RunShardsBody,
  ): Promise<{ batchId: number; inserted: number }> {
    const stamp = Date.now();
    const r = await this.queue.enqueueBatch({
      items: Array.from({ length: body.shards }, (_, i) => ({
        kind: "report.shard",
        payload: { scope: body.scope, shard: i },
        uniqueKey: `report:${body.scope}:${stamp}:${i}`,
      })),
      onComplete: {
        kind: "report.summary",
        payload: { scope: body.scope, runAt: stamp },
      },
      metadata: { scope: body.scope, runAt: stamp },
    });
    return { batchId: Number(r.batchId), inserted: Number(r.inserted) };
  }
}
