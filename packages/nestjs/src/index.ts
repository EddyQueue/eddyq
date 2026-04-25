// @eddyq/nestjs — NestJS module for eddyq.
//
// Public surface:
//   - EddyqModule.forRoot / forRootAsync
//   - @InjectEddyq() — DI for the Eddyq client
//   - @Processor() class decorator — marks a provider as a job processor
//   - @JobHandler(kind) method decorator — binds a method to a job kind
//
// Types and error classes from @eddyq/queue are re-exported for convenience
// so downstream code can `import { Eddyq, JobCall, CancelError } from "@eddyq/nestjs"`.

export const VERSION = "0.0.1";

export { EddyqModule } from "./eddyq.module.js";
export { EddyqExplorer } from "./eddyq.explorer.js";
export type { DiscoveredHandler } from "./eddyq.explorer.js";
export {
  InjectEddyq,
  JobHandler,
  Processor,
} from "./eddyq.decorators.js";
export {
  EDDYQ_INSTANCE,
  EDDYQ_OPTIONS,
} from "./eddyq.constants.js";
export type {
  EddyqModuleAsyncOptions,
  EddyqModuleOptions,
  JobHandlerFn,
} from "./eddyq.types.js";

export { CancelError, Eddyq, RetryError } from "@eddyq/queue";
export type {
  ConnectOptions,
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
