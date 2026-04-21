# Security Policy

## Supported versions

eddyq is in pre-alpha. Once v1.0 ships, the latest minor release will receive security updates.

| Version | Supported |
|---------|-----------|
| < 1.0   | :x: (pre-alpha, no security guarantees) |

## Reporting a vulnerability

**Please do not report vulnerabilities via public GitHub issues.**

Instead, email the maintainers at `security@eddyq.dev` (replace once domain is secured) with:

- A description of the vulnerability
- Steps to reproduce
- The impact (data exposure, availability, integrity)
- Your suggested mitigation, if any

We'll acknowledge receipt within 72 hours and follow up with a disclosure timeline. We request that you give us a reasonable window (typically 90 days) to release a fix before public disclosure.

## Scope

In scope:

- The `eddyq-core`, `eddyq-client`, `eddyq-cli`, and `eddyq-napi` crates
- The `@eddyq/queue`, `@eddyq/nestjs`, and `@eddyq/dashboard` packages
- SQL injection, auth bypass, or privilege escalation in the queue engine
- Any path to data exfiltration via a maliciously crafted job payload

Out of scope:

- Denial-of-service via enqueueing large volumes of legitimate jobs (operator responsibility)
- Issues in third-party dependencies (report upstream)
- Social-engineering attacks against maintainers
