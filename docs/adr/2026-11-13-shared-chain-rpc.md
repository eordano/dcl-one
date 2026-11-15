# ADR: Shared Ethereum RPC readers

- Status: Accepted
- Date: 2026-11-13

## Decision

The existing `catalyst-chain-index` crate owns Ethereum JSON-RPC log and header transport in its `rpc` module. LAND authorization and Shop consume that module directly. The client holds the outbound transport and endpoint; callers supply address and topic filters. It does not hold LAND registry settings.

The shared module owns response parsing, bounded log-range retries, header batches and generic transport, RPC and request-budget errors. LAND authorization retains registry filters, timestamp validity and batch retry policy, request accounting, cursor advancement, fold readiness and repair. Its error conversion retains domain error distinctions and messages. Shop retains receipt checks, index folds, reorganization handling and Credits settlement policy.

This keeps shared chain-reading mechanisms below domain policy without another crate, outbound client, pool or deployment. Existing layer dependencies already permit both consumers to use chain-index. No schema, HTTP route, authorization rule or request budget changes.

## Validation

The chain reader parser tests run in chain-index. LAND integration tests exercise range hints, budget exhaustion, missing timestamps, batch fallback and retry, replay and publication. Shop tests exercise its filters, reorganization boundaries and Credits index consumers. The module dependency check enforces the existing layer graph.
