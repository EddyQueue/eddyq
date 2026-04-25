---
layout: home

hero:
  name: eddyq
  text: Postgres-backed job queue
  tagline: Rust core. First-class Node bindings. The reliability of your database, the ergonomics you expect.
  actions:
    - theme: brand
      text: Get started
      link: /guide/
    - theme: alt
      text: View on GitHub
      link: https://github.com/EddyQueue/eddyq

features:
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2L4 6v6c0 5 3.5 9 8 10 4.5-1 8-5 8-10V6l-8-4z"/><path d="M9 12l2 2 4-4"/></svg>'
    title: Node.js
    details: Direct API for Express, Fastify, or any plain Node app. Install one package and you're ready to enqueue.
    link: /node/quick-start
    linkText: Node.js quick start
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2l9 5v10l-9 5-9-5V7l9-5z"/><path d="M12 12l9-5"/><path d="M12 12v10"/><path d="M12 12L3 7"/></svg>'
    title: NestJS
    details: First-class module with decorators, DI, and a forRoot config. Drops cleanly into your existing Nest app.
    link: /nestjs/setup
    linkText: NestJS setup
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>'
    title: Postgres-native
    details: No new infrastructure. Jobs live alongside your data with full transactional guarantees — enqueue inside the same transaction as your business write.
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M15 2v2M15 20v2M2 15h2M2 9h2M20 15h2M20 9h2M9 2v2M9 20v2"/></svg>'
    title: Rust core
    details: The hot path is Rust for predictable latency under load. NAPI-RS bindings keep the JS surface idiomatic — no FFI weirdness.
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>'
    title: Familiar API
    details: Queues, workers, schedules, retries, cancellation, group concurrency. Patterns you already know from BullMQ and Sidekiq.
  - icon: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8v4l3 3"/><circle cx="12" cy="12" r="9"/></svg>'
    title: Cron schedules
    details: Declarative, durable cron schedules backed by the same Postgres. Single MAINTENANCE_ROLE picks up missed runs after a deploy.
---
