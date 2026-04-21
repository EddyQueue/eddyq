# eddyq-client

Enqueue and admin client for [eddyq](https://github.com/eddyqueue/eddyq).

Use this crate when you want to enqueue jobs (or run admin operations like pause/resume groups) without pulling in the full worker engine. The `eddyq-cli` and `eddyq-napi` crates depend on this.
