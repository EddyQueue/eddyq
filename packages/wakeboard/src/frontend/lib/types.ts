export interface QueueStateCount {
  queue: string;
  state: string;
  count: number;
}

export interface Stats {
  byQueueState: QueueStateCount[];
}

export interface JobRow {
  id: number;
  queue: string;
  kind: string;
  state: string;
  priority: number;
  attempt: number;
  maxAttempts: number;
  scheduledAt?: string;
  createdAt: string;
  finalizedAt?: string;
  groupKey?: string;
  tags?: string[];
  payload: unknown;
  result?: unknown;
  errors?: unknown;
  metadata?: unknown;
}

export interface JobList {
  total: number;
  rows: JobRow[];
}

export interface NamedQueue {
  name: string;
  runningCount: number;
  maxConcurrency: number;
  paused: boolean;
  defaultTimeoutMs?: number;
  createdAt: string;
  updatedAt: string;
}

export interface Group {
  key: string;
  runningCount: number;
  maxConcurrency: number;
  paused: boolean;
  rateCount?: number;
  ratePeriodMs?: number;
  tokens: number;
  tokensRefilledAt?: string;
  createdAt: string;
  updatedAt: string;
}

export interface Schedule {
  name: string;
  kind: string;
  payload: unknown;
  cronExpr: string;
  nextRunAt: string;
  lastRunAt?: string;
  enabled: boolean;
  priority: number;
  maxAttempts: number;
}
