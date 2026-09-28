# V2 — `BinanceFuturesLive`: a `CexExecutor` for Binance USDⓈ-M perpetuals

Status: proposed. Depends on V1. Needs USDⓈ-M futures testnet keys, which are separate from the spot
testnet keys. About 3 days.

## Why

A consumer hedging a position held elsewhere needs to short the asset and resize that short often. The
spot adapters cannot do this. USDⓈ-M perpetuals are a different API from Binance spot: another host
(`fapi`), another symbol set, other filters, a position per symbol, reduce-only orders, and a fill
response that carries no commission. They get their own adapter. The trait does not change (`SPEC.md`
§6, last paragraph).

**Before building: can the account trade these at all?** Binance restricts USDⓈ-M futures by
jurisdiction (UK retail users, for example, since 2021). The testnet does not check. Confirm that the
production account is eligible before any of this is built.

## Layout

```text
src/cex/binance/
├── sign.rs              # HMAC-SHA256 query signing, moved out of rest.rs and shared by spot and futures
├── rest.rs              # spot, unchanged apart from using sign.rs
└── live.rs
src/cex/binance_futures/
├── mod.rs
├── rest.rs              # BinanceFuturesRest: signed fapi client, server-clock offset
├── filters.rs           # exchangeInfo → per-symbol MARKET_LOT_SIZE and MIN_NOTIONAL
└── live.rs              # BinanceFuturesLive: impl CexExecutor
```

## Configuration

```rust
pub struct BinanceFuturesConfig {
    pub base_url: String,     // BINANCE_FUTURES_BASE_URL; defaults to the testnet host, never production
    pub api_key: String,      // BINANCE_FUTURES_API_KEY, required
    pub api_secret: String,   // BINANCE_FUTURES_API_SECRET, required
}
```

Production is `https://fapi.binance.com`. The testnet host has moved before
(`https://testnet.binancefuture.com`, and since then Binance's "demo trading" hosts), so confirm it
when building and record it in the module docs. As with spot, pointing this adapter at the testnet
*is* its Simulated mode (`SPEC.md` §3). There is no separate struct.

## Construction checks the account once

```rust
impl BinanceFuturesLive {
    /// Connects, then refuses to return an adapter for an account it cannot
    /// trade correctly. `symbols` are the only symbols `execute` will accept.
    pub async fn connect(config: BinanceFuturesConfig, symbols: &[&str]) -> Result<Self>;
}
```

| Check | Endpoint | Refuses when | Why |
| --- | --- | --- | --- |
| Clock offset | `GET /fapi/v1/time` | the round trip is too slow to fit inside `recvWindow` | Signed calls carry a timestamp that the venue checks against its own clock (`SPEC.md` §6) |
| Position mode | `GET /fapi/v1/positionSide/dual` | `dualSidePosition` is `true` | Hedge mode does not accept `reduceOnly`, and it keeps two positions per symbol |
| Single-asset margin | `GET /fapi/v1/multiAssetsMargin` | `multiAssetsMargin` is `true` | V4's margin read reports in one asset |
| BNB fee payment | `GET /fapi/v1/feeBurn` | `feeBurn` is `true` | Commission could then arrive in BNB and the margin asset within one order, and `CexFill` holds one asset ([README](README.md), defect 2) |
| Symbols | `GET /fapi/v1/exchangeInfo` (public) | a symbol is missing, not `TRADING`, or not `PERPETUAL` | Refuse at start, not on the first order |

Margin type and leverage per symbol are **read and not set**. Changing account configuration is not
executing a decided action, and a consumer that needs a particular setup checks it with V4's
`PerpPosition.margin_mode` and `leverage` before it trades.

The clock offset is refreshed every 10 minutes, and once immediately if the venue answers `-1021`
(timestamp outside `recvWindow`). In that case the order is retried once with the new offset. A `-1021`
means the order was not accepted, so the retry cannot duplicate it.

## `execute`

1. **Validate before sending.** The symbol must be one given to `connect`. Round `quantity` **down**
   to `MARKET_LOT_SIZE.stepSize`. Refuse quantities below `minQty` or above `maxQty`. Refuse when
   `quantity × quoted_price` is below `MIN_NOTIONAL.notional`: `quoted_price` is only the estimate for
   this check and never part of the order. **Skip the notional check for reduce-only orders.**
   Binance's own error for it (`-4164`) reads "Order's notional must be no smaller than … (unless you
   choose reduce only)", and a small remaining position must always be closable. Confirm this on the
   testnet.
2. **Place.** `POST /fapi/v1/order` with `type=MARKET`, `side`, `quantity`, `reduceOnly`,
   `newOrderRespType=RESULT`, and a `newClientOrderId` that the adapter generates, unique per call.
3. **Read the result.**
   - `FILLED`: `filled_qty = executedQty`, `filled_price = avgPrice`.
   - `EXPIRED` or `CANCELED` with `executedQty > 0`: a partial fill, returned as a `CexFill`. It is
     never an error.
   - `NEW` or `PARTIALLY_FILLED`: poll `GET /fapi/v1/order?origClientOrderId=` until the order reaches
     a terminal state, then as above.
   - An error code in the response (`-2022` "ReduceOnly Order is rejected", `-4164` notional, `-2019`
     margin): the order was refused and nothing filled. Return a plain error carrying the venue's code
     and message.
4. **Commission.** The `RESULT` response has none. Read `GET /fapi/v1/userTrades?symbol=&orderId=` and
   add up `commission` over the lines. There must be exactly one `commissionAsset`. The lines can lag
   the fill by a moment, so retry for up to 2 s. If they have not appeared by then, return
   `OrderStateUnknown` with the order id. **Never return a fill with a guessed commission.**
5. **Lost response.** If the placing call times out or the connection drops, query
   `GET /fapi/v1/order?origClientOrderId=`.
   - Found: continue from step 3.
   - `-2013` (order does not exist) once `recvWindow` has passed: the venue never accepted the order,
     so return a plain error. The caller may retry.
   - The query itself fails: return `OrderStateUnknown { client_order_id, order_ref: None }`.

`CexFill.order_ref` is the venue's `orderId`, and `provenance` is always `Landed`, on the testnet too.
The testnet sends a real order to a sandbox venue: that matches how the spot adapters label testnet
fills, and `SPEC.md` §3 treats the sandbox as the Simulated environment, not as a throwaway run.

## Tests

- `wiremock`: signing, and the query order for signed POSTs. Each `connect` refusal. Rounding at the
  step, and refusal below the minimum quantity and notional. `FILLED`, partial `EXPIRED`, and `NEW`
  followed by a poll. `-2022` as a plain error. Commission lines arriving on the second poll.
  Commission lines never arriving, which gives `OrderStateUnknown`. A dropped placing call recovered by
  the client order id. A dropped placing call followed by a failed query, which gives
  `OrderStateUnknown`.
- `cex_executor_contract` against the testnet, gated on `BINANCE_FUTURES_API_KEY` being set, like the
  Sepolia tests.
- The `SPEC.md` §9.2 bar on the testnet: 100 orders of distinct sizes, each reconciled to the cent
  against `userTrades`, plus one injected failure per variant: a rejected reduce-only order, a
  notional refusal, and `OrderStateUnknown` forced by dropping the connection after sending.

## Done when

The §9 bar is met on the testnet. `SPEC.md` §8 lists `cex/binance_futures/`. The module docs record
the testnet host, that reduce-only orders are exempt from the notional minimum, and what the venue
does with a reduce-only order larger than the position (which `CexStub` then copies, per V1).
