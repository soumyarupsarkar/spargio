# Placement and Stealing

Core placement policies:

- `Pinned(shard)`
- `RoundRobin`
- `Sticky(key)`
- `Stealable`
- `StealablePreferred(shard)`.

Use `Pinned` for strict shard-local state, and `Stealable*` when post-I/O CPU
work can move for better balance.
