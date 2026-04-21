import { JobCall as NativeJobCall } from "./index.js";

// Re-export everything except Eddyq (which we re-declare below with an
// augmented `work` signature via declaration merging).
export * from "./index.js";
export { Eddyq } from "./index.js";

/**
 * `JobCall` augmented with an `AbortSignal` attached by the lib.cjs wrapper.
 * The signal is triggered when `eddyq.shutdown()` is called, so long-running
 * handlers (e.g. `fetch`) can pass it along and bail cleanly.
 *
 * @example
 * eddyq.work("download", async ({ payload, signal }) => {
 *   const res = await fetch(payload.url, { signal });
 *   return res.json();
 * });
 */
export interface JobCall extends NativeJobCall {
  signal: AbortSignal;
}

// Declaration-merge an overload onto Eddyq.work so handlers can accept the
// augmented `JobCall` (with `signal`). The original signature from index.d.ts
// remains, so both are valid call shapes.
declare module "./index.js" {
  interface Eddyq {
    work(kind: string, handler: (call: JobCall) => Promise<unknown>): void;
  }
}

/**
 * Throw from a handler to permanently fail a job — the queue will NOT retry,
 * regardless of `maxAttempts`.
 *
 * @example
 * eddyq.work("process", async (call) => {
 *   if (!isValid(call.payload)) throw new CancelError("invalid input");
 * });
 */
export class CancelError extends Error {
  constructor(message?: string);
}

/** Options for {@link RetryError}. */
export interface RetryErrorOptions {
  /** Milliseconds until the next attempt. Preferred. */
  delayMs?: number;
  /** Alias for `delayMs`. */
  inMs?: number;
}

/**
 * Throw to retry at a custom delay instead of the default exponential backoff.
 * Useful for respecting upstream rate-limit headers (e.g. `Retry-After`).
 *
 * @example
 * eddyq.work("send", async (call) => {
 *   const resp = await fetch(url);
 *   if (resp.status === 429) {
 *     const after = Number(resp.headers.get("retry-after")) * 1000;
 *     throw new RetryError("rate limited", { delayMs: after });
 *   }
 * });
 */
export class RetryError extends Error {
  constructor(message?: string, opts?: RetryErrorOptions);
}
