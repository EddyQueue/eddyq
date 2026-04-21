// DI tokens + reflection metadata keys for @eddyq/nestjs.
//
// String tokens (not Symbols) so that users who inject by token name in tests
// or custom providers can type them directly.

export const EDDYQ_OPTIONS = "EDDYQ_OPTIONS";
export const EDDYQ_INSTANCE = "EDDYQ_INSTANCE";

export const EDDYQ_PROCESSOR_META = "eddyq:processor";
export const EDDYQ_JOB_HANDLER_META = "eddyq:job_handler";
