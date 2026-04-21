import { Injectable } from '@nestjs/common';
import { InjectEddyq } from '@eddyq/nestjs';
import type { Eddyq, ListJobsFilter, Pagination } from '@eddyq/queue';

@Injectable()
export class BoardService {
  constructor(@InjectEddyq() private readonly q: Eddyq) {}

  getStats() { return this.q.getStats(); }
  listJobs(f?: ListJobsFilter, p?: Pagination) { return this.q.listJobs(f, p); }
  listQueues() { return this.q.listNamedQueues(); }
  listGroups() { return this.q.listGroups(); }
  listSchedules() { return this.q.listSchedules(); }

  cancelJob(id: number) { return this.q.cancel(id); }
  pauseQueue(name: string) { return this.q.pauseQueue(name); }
  resumeQueue(name: string) { return this.q.resumeQueue(name); }
  setQueueConcurrency(name: string, max: number) { return this.q.setQueueConcurrency(name, max); }
  pauseGroup(key: string) { return this.q.pauseGroup(key); }
  resumeGroup(key: string) { return this.q.resumeGroup(key); }
  setGroupConcurrency(key: string, max: number) { return this.q.setGroupConcurrency(key, max); }
  setGroupRate(key: string, n: number, ms: number) { return this.q.setGroupRate(key, n, ms); }
  clearGroupRate(key: string) { return this.q.clearGroupRate(key); }
  setScheduleEnabled(name: string, enabled: boolean) { return this.q.setScheduleEnabled(name, enabled); }
  removeSchedule(name: string) { return this.q.removeSchedule(name); }
}
