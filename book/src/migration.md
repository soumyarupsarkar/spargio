# Migration Notes

From Tokio:

- choose placement policy explicitly (`Pinned`, `Stealable*`) where hot paths
  benefit from shard-awareness.
- for native disk/network data paths, prefer Spargio fs/net/native APIs.
- for protocol stacks, prefer `spargio-tls` / `spargio-ws` / `spargio-quic`
  when you want spargio-aligned timeout/cancel semantics.

From Compio/monoio:

- use existing fs/net/io ergonomics first.
- use extension wrappers when an operation is not yet in core high-level APIs.
- keep optional higher-level protocols in companion crates, with direct
  upstream crates reserved for cases where you need immediate upstream-only
  surface area.
