# Native Extensions

Spargio exposes low-level unsafe extension hooks:

- `UringNativeAny::submit_unsafe(...)`
- `UringNativeAny::submit_unsafe_on_shard(...)`.

Prefer safe wrappers over direct unsafe call sites in applications.

Current safe-wrapper example:

- `spargio::extension::fs::statx*`

See `docs/native_extension_cookbook.md` for wrapper invariants and patterns.
