# Companion Protocols

Spargio keeps high-level protocol stacks out of core and provides thin
runtime-aware companion adapters:

- `spargio-tls`: `rustls` + `futures-rustls`.
- `spargio-ws`: `async-tungstenite`.
- `spargio-quic`: `quinn` bridge execution lane.
- `spargio-protocols`: legacy blocking bridge helpers for ecosystem closures.

## Direct Upstream vs `spargio-*` Adapter

Use direct upstream crates when:

- you already have runtime-neutral glue and don't need spargio-specific timeout
  or placement conventions.
- you need the newest upstream surface immediately.

Use `spargio-*` adapters when:

- you want timeout/cancel behavior aligned with spargio companion semantics.
- you want one canonical integration path in a spargio codebase.
- you want examples/migration guidance tied to spargio APIs.

## Scope Boundary

Companion crates are intended to be thin integration layers, not replacement
protocol engines. Crypto/protocol internals remain upstream.
