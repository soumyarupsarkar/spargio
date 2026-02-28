# Native Extension Cookbook

This guide documents safe-wrapper patterns for extending Spargio's low-level
`submit_unsafe` lane without exposing `unsafe` to end users.

## Pattern 1: Own all SQE-backed state until completion

- Keep buffers, paths, and output structs inside an owned state struct.
- Build SQE pointers from that state.
- Decode CQE into a typed output and return it.

Example in core:

- `spargio::extension::fs::statx_on_shard(...)`

The wrapper stores `CString` + `statx` output buffer in state and only exposes a
typed `StatxMetadata`.

## Pattern 2: Separate affinity policy from operation shape

- Provide explicit-shard variant for deterministic placement.
- Provide selector/default variant for convenience.

Example:

- `statx_on_shard(native, shard, path, options)`
- `statx(native, path)` (selector-driven)

## Pattern 3: Add compatibility fallback in wrapper boundary

- Treat unsupported opcodes (`EINVAL` / `ENOSYS` / `EOPNOTSUPP`) as fallback
  conditions.
- Keep fallback inside the safe API boundary.

Example:

- `statx_or_metadata(handle, path)`:
  - native `statx` first
  - fallback to blocking `std::fs::metadata` bridge.

## Safety checklist

Use this checklist for every wrapper:

1. State owns all memory referenced by SQE pointers until completion.
2. CQE result is checked and converted before reading output buffers.
3. Any `unsafe` block is localized to wrapper internals.
4. Unsupported-kernel behavior is defined (fallback, explicit error, or both).
5. Wrapper API returns typed results and hides raw CQE/SQE details.
