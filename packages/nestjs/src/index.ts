// @eddyq/nestjs — NestJS module for eddyq.
//
// Public surface:
//   - EddyqModule.forRoot / forRootAsync — connection + worker runtime (once per app)
//   - EddyqModule.registerQueue — per-queue defaults, groups, schedules (per feature module)
//   - @InjectEddyq() — DI for the Eddyq client
//   - @InjectQueue(name) — DI for a per-queue QueueHandle
//   - @Processor() class decorator — marks a provider as a job processor
//   - @JobHandler(kind) method decorator — binds a method to a job kind
//
// Types and error classes from @eddyq/queue are re-exported for convenience
// so downstream code can `import { Eddyq, JobCall, CancelError } from "@eddyq/nestjs"`.

export { EddyqModule } from "./eddyq.module.js";
export { EddyqExplorer } from "./eddyq.explorer.js";
export type { DiscoveredHandler } from "./eddyq.explorer.js";
export { EddyqQueueAggregator } from "./eddyq-queue.aggregator.js";
export {
  InjectEddyq,
  InjectQueue,
  JobHandler,
  Processor,
} from "./eddyq.decorators.js";
export {
  EDDYQ_INSTANCE,
  EDDYQ_OPTIONS,
  getQueueToken,
  getQueueRegistrationToken,
} from "./eddyq.constants.js";
export type {
  EddyqInstance,
  EddyqModuleAsyncOptions,
  EddyqModuleOptions,
  EddyqQueueRoute,
  EddyqRedisOptions,
  EddyqTuningOptions,
  GroupProfile,
  GroupRate,
  GroupedQueueHandle,
  JobHandlerFn,
  QueueDefaults,
  QueueEnqueueBatchInput,
  QueueEnqueueManyItem,
  QueueEnqueueOptions,
  QueueHandle,
  QueueRegistration,
} from "./eddyq.types.js";

export { CancelError, Eddyq, EddyqApp, EddyqRedis, RetryError } from "@eddyq/queue";
export type {
  BatchEnqueueOutcome,
  BulkEnqueueOutcome,
  ConnectOptions,
  EnqueueBatchInput,
  EnqueueManyItem,
  EnqueueOptions,
  EnqueueOutcome,
  Group,
  JobCall,
  JobList,
  JobRow,
  JobStats,
  ListJobsFilter,
  MigrateReport,
  MigrationStatus,
  NamedQueue,
  Pagination,
  QueueStateCount,
  Schedule,
  ScheduleDeclaration,
  StartOptions,
} from "@eddyq/queue";
