# Stability Policy

This chapter describes what users can expect from Spargio and companion-crate versioning.

## Versioning Expectations

- Breaking API changes use a major version bump for the affected crate.
- Additive APIs ship in minor releases.
- Patch releases are for fixes and low-risk behavioral corrections.

## Behavior Compatibility

For async APIs, behavior is part of the contract. In particular:

- timeout behavior
- cancellation behavior
- error mapping/typing

If these change materially, you should treat it as semver-relevant.

## Deprecation Policy

- APIs are deprecated before removal.
- Removals happen on a major release.
- Compatibility shims may be kept temporarily to ease migration.

## Companion Crates

Companion crates may release independently of core runtime, so check each crate's version and changelog when upgrading:

- `spargio-tls`
- `spargio-ws`
- `spargio-quic`
- `spargio-process`
- `spargio-signal`
- `spargio-protocols`

## Upgrade Guidance

When upgrading:

1. read changelogs for all upgraded crates
2. re-run your integration and load tests
3. validate timeout/cancel/error behavior in critical flows
