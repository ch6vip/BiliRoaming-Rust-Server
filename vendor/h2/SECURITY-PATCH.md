# h2 0.3.27 security backport

Source: https://static.crates.io/crates/h2/h2-0.3.27.crate
SHA-256: `0beca50380b1fc32983fc1cb4587bfa4bb9e78fc259aad4a0032d2080309222d` (verified against Cargo.lock before extraction).
License: MIT (LICENSE retained).

Backport of upstream commit [193833e87c639e39751f339a9c375a44c8bfab54](https://github.com/hyperium/h2/commit/193833e87c639e39751f339a9c375a44c8bfab54), released in h2 0.4.16, for RUSTSEC-2026-0258 / GHSA-q83h-524g-xf6h.

Actix HTTP 3.18.12 still uses HTTP types 0.2 and h2 0.3. Upgrading its h2 dependency to 0.4 is not API compatible. This patch retains 0.3 interfaces and backports the official 256-byte overhead threshold, bounded 25,600-byte connection budget, budget return on consumed DATA, dropping non-final empty DATA events, and closing over-budget connections with ENHANCE_YOUR_CALM. Budget is also returned when a stream's receive buffer is dropped without the application reading it (`clear_recv_buffer`/`release_closed_capacity`), matching 0.4.19: without that, a server that responds before consuming the whole request body leaks budget and eventually closes healthy connections. h2 0.3 uses decoded payload length for flow control, so its existing accounting is preserved rather than importing 0.4's unrelated padding changes.

Closed streams also clear their receive buffer when the payload byte count is zero, which requires dropping the `in_flight_recv_data == 0` early return in `release_closed_capacity`.

The original crate version is retained; no advisory is silently ignored. The dependency audit scans this path dependency as h2 0.3.27 and records this specific audited backport separately. Future advisories for h2 still fail the audit. Remove the override when Actix adopts a fixed upstream h2 release.

Only src/, Cargo.toml, LICENSE, README.md and CHANGELOG.md are vendored. Cargo.toml example entries are removed because examples aren't included. The protocol tests live in the application's tests/network_security.rs.
