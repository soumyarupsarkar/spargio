# Runtime Entry

Preferred entrypoints:

- `spargio::run(...)`
- `spargio::run_with(builder, ...)`
- optional `#[spargio::main(...)]` (feature `macros`).

Key builder controls:

- shard count
- message queue capacity knobs
- steal queue/budget knobs
- optional thread affinity mapping.
