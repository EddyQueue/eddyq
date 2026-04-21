declare global {
  interface Window { __EDDYQ_API_BASE__: string }
}

const base = (): string => window.__EDDYQ_API_BASE__ ?? '/board';

async function req<T>(path: string, method = 'GET'): Promise<T> {
  const res = await fetch(`${base()}${path}`, { method });
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status}`);
  return res.json() as Promise<T>;
}

export const getStats = () => req<import('./types.js').Stats>('/api/stats');
export const listJobs = (params: Record<string, string> = {}) => {
  const qs = new URLSearchParams(params).toString();
  return req<import('./types.js').JobList>(`/api/jobs?${qs}`);
};
export const listQueues = () => req<import('./types.js').NamedQueue[]>('/api/queues');
export const listGroups = () => req<import('./types.js').Group[]>('/api/groups');
export const listSchedules = () => req<import('./types.js').Schedule[]>('/api/schedules');

export const cancelJob = (id: number) => req(`/api/jobs/${id}/cancel`, 'POST');
export const pauseQueue = (name: string) => req(`/api/queues/${encodeURIComponent(name)}/pause`, 'POST');
export const resumeQueue = (name: string) => req(`/api/queues/${encodeURIComponent(name)}/resume`, 'POST');
export const pauseGroup = (key: string) => req(`/api/groups/${encodeURIComponent(key)}/pause`, 'POST');
export const resumeGroup = (key: string) => req(`/api/groups/${encodeURIComponent(key)}/resume`, 'POST');
export const enableSchedule = (name: string) => req(`/api/schedules/${encodeURIComponent(name)}/enable`, 'POST');
export const disableSchedule = (name: string) => req(`/api/schedules/${encodeURIComponent(name)}/disable`, 'POST');
export const removeSchedule = (name: string) => req(`/api/schedules/${encodeURIComponent(name)}/remove`, 'POST');
