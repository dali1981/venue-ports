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
| DEX (EVM) | done (`EvmStub`) | verified against a real Sepolia swap (Uniswap V3 `SwapRouter02`) — §9.2's full 100-run replication not yet done | verified against a real, successful Sepolia swap — quote/simulated/executed agree to the wei; §9.2's full 100-run replication not yet done |
| CEX — Binance | done (`CexStub`) | testnet credentials in hand; blocked on Binance's own geo-eligibility check (HTTP 451) from this environment | scaffolded (`BinanceLive`), unit-tested against a mocked server, not yet run against the real testnet |
| CEX — Bybit | done (`CexStub`) | blocked on Bybit's own CloudFront geo-restriction from this environment | scaffolded (`BybitLive`), unit-tested against a mocked server, not yet run against the real testnet |
| CEX — Binance USDⓈ-M futures | done (`CexStub`, with a reduce-only mode) | needs futures testnet keys; the gated §9.2 run is written | built (`BinanceFuturesLive`), unit-tested against a mocked server, not yet run against the real testnet |
| Liquidity (EVM position managers) | done (`LiquidityStub`) | `EvmLiquidity` over a fork sender meets §9.2 for Uniswap v3 on anvil (100 lives reconciled to the wei); Slipstream needs a Base fork | built (`EvmLiquidity` over a signing sender); no live life taken yet |
| Perp account reads | done (`CexAccountStub`) | needs futures testnet keys; the gated run is written | built (`BinanceFuturesAccount`), unit-tested against a mocked server |
| Balance reads, resolving an unknown swap or order (V6, phase 16) | done (`EvmBalanceStub`, `SpotBalanceStub`, `CexStub`) | `against_anvil_*` tests written, gated on `EVM_ANVIL_RPC_URL`, not yet run | built (`EvmBalances`, spot account reads on `BinanceRest`/`BinanceLive`, `EvmLive::resolve`, `CexOrders` on `BinanceLive`), unit-tested against mocked servers on the documentation's bodies; a signing sender on anvil is `Simulated` |
| Production validation of Binance spot and router swaps (V7, phase 17) | done: every case runs against stateful mocks (`MiniExchange`, `MiniChain`) and `wiremock`, passing and failing | not applicable: it validates `Live` | **written, offline-tested, not run against a production venue.** Four `#[ignore]`d tests (`production_lv1_binance`, `production_lv1_base`, `production_lv2a_exchange`, `production_lv2b_chain`), a typed `VenueRefusal`, six Binance spot reads, a guard, a spend ledger and a halt file; a response catalogue (`docs/responses/`) whose bodies are all `documented` or `synthetic` until a person runs them |

Every `Stub` implementation passes the shared contract-test suites (`src/testkit/contract.rs`) and is
ready to build a trading system against today. See [`examples/basic_usage.rs`](examples/basic_usage.rs)
for a minimal sketch of calling the swap and order ports and handling what comes back, and
[`examples/liquidity_lifecycle.rs`](examples/liquidity_lifecycle.rs) for a position's whole life through
the liquidity port.

Neither `Live` adapter is exercised by an automated test or a schedule — per `SPEC.md` §3/§9, that is
only ever a deliberate, human-triggered action.

**Every `Live` adapter is testnet-first by default** — `EvmLive` against Ethereum Sepolia,
`BinanceLive`/`BybitLive` against each venue's own testnet host. `EvmLive` has now run a real,
successful, human-triggered swap on Sepolia (see the Status table above); `BinanceLive`/`BybitLive`
remain blocked on input only a human can give: real testnet credentials, and (from inside this
environment specifically) each venue's own geo-restriction. See the "BLOCKED" section at the top of
[`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md#blocked--everything-below-needs-input-only-you-can-give)
for exactly what is needed and why nothing further can proceed without it.

**Added from [`specs/`](specs/README.md)** (phases 10–14 in the plan): one shared EVM sender per
wallet and chain, reduce-only orders and `OrderStateUnknown`, a Binance USDⓈ-M futures adapter, a
liquidity port for concentrated-liquidity position managers, and read-only perp account queries.

**Running the EVM tests against a real node, without a fork RPC:** start `anvil
--disable-code-size-limit`, then `eval "$(scripts/anvil-uniswap-v3.py)"` deploys Uniswap's published
v3 bytecode onto it and exports what the anvil-gated tests read; `cargo test -- against_anvil` runs
them. CI's `anvil` job does the same on every push and nightly.

## Scope, in one paragraph

Given a route or an order that something else has already decided to send, this crate is responsible
for turning it into a real (or simulated, or faked) outcome and returning that outcome once it is
known — nothing more. It does not fetch price quotes, decide whether a trade is worth taking, track
positions or capital, or record anything to a database. Those are a trading system's own concerns and
stay out of this crate by design (see "Non-goals" in `SPEC.md`).

## License

MIT — see [`LICENSE`](LICENSE).
