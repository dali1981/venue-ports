# Test profile — venue-ports

Written 23 October 2026 at 6173283 (branch `node-errors-without-url`). Stale when a package, a major dependency or the build or
test configuration changes.

One package, one language (Rust). Every test runs offline: node calls go to a local `wiremock` server. Live tests are gated by
environment variables and skip without them.

| Form | Command | Measured |
| --- | --- | --- |
| Single test | `nice -n 10 cargo test --offline --manifest-path ~/trading_project/venue-ports/Cargo.toml --lib -j 2 <filter> -- --test-threads 2` | ~1 s warm |
| Fast check | `nice -n 10 cargo check --offline --manifest-path ~/trading_project/venue-ports/Cargo.toml --all-targets -j 2`, and again with `--features production` | ~10 s each |
| Package pass | `nice -n 10 cargo test --offline --manifest-path ~/trading_project/venue-ports/Cargo.toml -j 2 -- --test-threads 2` | 642 passed, 5 ignored, 19 s |

Heavy: none.

Conventions: `SPEC.md` (the ports and each adapter's contract), `IMPLEMENTATION_PLAN.md`, `README.md`. A node's URL never appears
in an error or a `Debug` (`src/node_url.rs`).
