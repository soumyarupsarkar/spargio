# Companion Crate Contract (Phase 1)

This document captures the baseline contract for `spargio-*` companion crates.

## Scope

- Protocol bridges: `spargio-protocols` (`tls`, `ws`, `quic` blocking bridges).
- Process bridge: `spargio-process`.
- Signal bridge: `spargio-signal` (expanded in later phases).

## Shared Semantics

- Runtime rejection (`RuntimeError`) is mapped to `std::io::Error`:
  - `InvalidConfig` -> `InvalidInput`
  - `InvalidShard` -> `NotFound`
  - `Closed` -> `BrokenPipe`
  - `Overloaded` -> `WouldBlock`
  - `UnsupportedBackend` -> `Unsupported`
  - native runtime I/O setup failures -> forwarded `io::Error`
- Task cancellation (`JoinError::Canceled`) is mapped to `BrokenPipe`.
- Timeout enforcement uses `spargio::timeout(...)` and maps to `TimedOut`.

## API Patterns

- Protocol bridges expose optioned variants:
  - `*_blocking(...)`
  - `*_blocking_with_options(..., BlockingOptions, ...)`
- Process bridge exposes optioned variants:
  - `status(...)`, `output(...)`
  - `status_with_options(..., CommandOptions)`,
    `output_with_options(..., CommandOptions)`
- Timeout options are opt-in and default to no timeout.

## io Compatibility Baseline

For Linux with `uring-native`, `spargio-protocols::io_compat::FuturesTcpStream`
provides a `futures::io::{AsyncRead, AsyncWrite}` adapter over
`spargio::net::TcpStream` so protocol crates can compose with
`futures-rustls`/`async-tungstenite` style APIs without core-crate changes.
