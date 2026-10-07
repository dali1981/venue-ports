# Proposed changes

**Status: V0–V4 are accepted into `SPEC.md` and built. V5 is accepted (1 October 2026) and copied
into `SPEC.md`; it is phase 15, in progress. V6 (7 October 2026) is copied into `SPEC.md` and is phase 16,
on branch `m10c-balances-resolution`.** V0–V4 are phases 10–14 of `IMPLEMENTATION_PLAN.md`,
which records what each still needs before it counts as done — mostly testnet keys and a Base fork.

Each file here specifies one change to the crate, to be built in this repository. `SPEC.md` stays
the single contract. Accepting a spec means copying its signatures into `SPEC.md` and adding its
phase to `IMPLEMENTATION_PLAN.md`. That is the first step of implementing it, before any code.

These were requested on 28 September 2026 by a consumer that provides concentrated liquidity on EVM
pools and hedges it with perpetual futures. None of the specs uses that consumer's vocabulary. Each
turns an action the consumer has already decided on into an outcome, which is all this crate does
(`SPEC.md` §2).

| Spec | Adds | Depends on | Needs | Days |
| --- | --- | --- | --- | --- |
| [V0](V0-evm-shared-sender.md) | One EVM sender per wallet and chain, shared by every EVM adapter, so two adapters can never race for a nonce. The router's return is decoded from its first word | — | nothing | 1 |
| [V1](V1-order-contract.md) | `OrderRequest.reduce_only`, and a typed error that means "the venue may have filled this" | — | nothing | 1 |
| [V2](V2-binance-usdm-futures.md) | `BinanceFuturesLive`, a `CexExecutor` for Binance USDⓈ-M perpetuals | V1 | futures testnet keys | 3 |
| [V3](V3-liquidity-port.md) | The liquidity port: mint, increase, decrease, collect and burn on concentrated-liquidity position managers, with stub, fork-simulated and live adapters | V0 | an anvil fork (Simulated); a funded testnet key (Live) | 6.5 |
| [V4](V4-cex-account-reads.md) | `CexAccount`, a read-only port for a perp position, margin and funding as the venue reports them | V2's REST client | futures testnet keys | 1.5 |
| [V5](V5-venues-and-solana.md) | One contract per port for every venue (commands, events, capabilities); `Prepared` as an enum; `Network`; per-family cost; the Solana family; Jupiter and Orca Whirlpool | V3 | Surfpool (Simulated) | — |
| [V6](V6-balances-and-resolution.md) | `EvmBalanceReader` and `SpotBalanceReader`; `EvmLive::resolve` and `Resolution`; `CexOrders::order_state`; a signing sender on anvil is `Simulated` | V0, V1 | anvil (gated tests) | 3 |
| **Total (V0–V4)** | | | | **13** |

## Order

1. **No network needed:** V0, V1 and V3's `LiquidityStub`. The consumer writes its code against these.
2. **With futures testnet keys:** V2 and V4 together. They share one REST client and one set of keys.
3. **With a fork and then a funded testnet key:** V3's `EvmLiquidity`, first over a fork sender and then over a signing one.

## Base and releases

These specs were written against `cex-venues-binance-bybit` at `7427e40`, which is not yet merged
into `main`. They refer to `BinanceLive`, `dex::evm::tx::Signer` and `EvmLive` as that branch has
them.

The crate has no tag. Consumers pin a tag, never a branch, so:

- `v0.1.0`: when `cex-venues-binance-bybit` is merged into `main`.
- `v0.2.0`: when V0, V1 and `LiquidityStub` are merged. This is the first version the requesting
  consumer pins.

## Defects found while reading for these specs

These belong to no spec. Each is small and can be fixed on its own. **All four are fixed** (1 by V0,
2–4 alongside V1's error rule for the spot adapters); defect 4's venue check on the Spot Testnet is
still to be made.

1. **`EvmSimulated` panics on any router that returns more than one word.** `execute` decodes the
   whole return with `U256::from_be_slice`, which panics when the value exceeds 256 bits. KyberSwap's
   `MetaAggregationRouterV2.swap` returns `(uint256 returnAmount, uint256 gasUsed)`, 64 bytes with a
   non-zero first word, so every KyberSwap route panics the adapter. The Sepolia check that confirmed
   the single-`uint256` convention used `SwapRouter02.exactInputSingle`, which returns one word. V0
   fixes this.
2. **`BinanceLive` under-reports commission paid in more than one asset.** `fill_from_response` sums
   commission only in the first fill's asset and counts every other asset's lines as zero. Its comment
   says it "flags rather than silently drops"; it drops them. `CexFill` carries one commission asset, so
   the fix is either a list of commissions (a trait change for every venue) or a check at start that
   refuses an account with BNB fee payment switched on. V2 takes the second route for futures.
3. **`BinanceLive` does not synchronise its clock, and has no status query to fall back on.** It signs
   with the local clock and never reads `GET /api/v3/time`, which `SPEC.md` §6 requires. If the
   response to the placing call is lost, the adapter has nothing to query. V2 does both for futures,
   and spot needs the same fixes.
4. **`BinanceLive` reports an error for a market order that partly filled and then expired.** Any
   status other than `FILLED` or `PARTIALLY_FILLED` becomes an error, even when `fills` is non-empty
   (for example `EXPIRED` after the book ran out). The caller is then told that nothing filled when
   something did. Confirm on the testnet that Binance returns this status for a partial market fill.
   Once V1 lands, return the partial fill as a `CexFill`.
