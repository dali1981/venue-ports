# venue-ports

A Rust crate that provides a **tested, reusable connection layer** between an automated trading
system and the venues it trades on — decentralized exchange (DEX) routers on EVM and Solana-style
chains, and centralized exchange (CEX) trading APIs.

It exists so the crate can be built once, correctly, and reused by more than one trading project,
instead of every project re-deriving its own signing, order-rounding, and fill-handling code.

Read [`SPEC.md`](SPEC.md) for the full technical specification — architecture, the exact Rust trait
and type signatures to implement, module layout, the three-mode execution model (live / simulated /
stub), the test strategy that keeps the stub honest, and acceptance criteria. See
[`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) for the phased build-out and current status.

## Status

| port | `Stub` | `Simulated` | `Live` |
|---|---|---|---|
| DEX (EVM) | done (`EvmStub`) | verified against a real Sepolia swap (Uniswap V3 `SwapRouter02`) — §9.2's full 100-run replication not yet done | scaffolded (`EvmLive`), not yet run — needs a signer |
| CEX — Binance | done (`CexStub`) | testnet credentials in hand; blocked on Binance's own geo-eligibility check (HTTP 451) from this environment | scaffolded (`BinanceLive`), unit-tested against a mocked server, not yet run against the real testnet |
| CEX — Bybit | done (`CexStub`) | blocked on Bybit's own CloudFront geo-restriction from this environment | scaffolded (`BybitLive`), unit-tested against a mocked server, not yet run against the real testnet |

Both `Stub` implementations pass the shared contract-test suite (`src/testkit/contract.rs`) and are
ready to build a trading system against today. See [`examples/basic_usage.rs`](examples/basic_usage.rs)
for a minimal sketch of calling both ports and handling what comes back.

Neither `Live` adapter is exercised by an automated test or a schedule — per `SPEC.md` §3/§9, that is
only ever a deliberate, human-triggered action.

**Every `Live` adapter is testnet-first by default** — `EvmLive` against Ethereum Sepolia,
`BinanceLive`/`BybitLive` against each venue's own testnet host — and is currently blocked on input
only a human can give: real credentials, and (from inside this environment specifically) broader
network egress. See the "BLOCKED" section at the top of
[`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md#blocked--everything-below-needs-input-only-you-can-give)
for exactly what is needed and why nothing further can proceed without it.

## Scope, in one paragraph

Given a route or an order that something else has already decided to send, this crate is responsible
for turning it into a real (or simulated, or faked) outcome and returning that outcome once it is
known — nothing more. It does not fetch price quotes, decide whether a trade is worth taking, track
positions or capital, or record anything to a database. Those are a trading system's own concerns and
stay out of this crate by design (see "Non-goals" in `SPEC.md`).

## License

MIT — see [`LICENSE`](LICENSE).
