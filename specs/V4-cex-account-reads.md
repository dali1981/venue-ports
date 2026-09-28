# V4 — `CexAccount`: read what the venue reports about a perp account

Status: proposed, and waiting on one decision by the owner: do account reads belong in this crate at
all? The alternative is that each consumer reads its own account. This spec is written so that it can
be accepted as it stands. Depends on V2's REST client and keys. About 1.5 days.

## Why

A consumer holding a perp position needs three facts only the venue knows:

- the position the venue holds, to check against the consumer's own view of it;
- the account's margin, so it can shrink before it is liquidated;
- the funding it paid or received, for its books.

Each consumer would otherwise write its own signed client for three endpoints, next to the signed
client this crate already has for the same host and keys.

## The line this draws in `SPEC.md` §2

§2 says the crate does not "track positions, balances, or capital". It changes to:

> track positions, balances, or capital — hold, compute, or cache them — or enforce any risk policy.
> *Reading* what a venue reports about an account, at the moment it is asked, is not tracking:
> nothing is kept between calls and nothing is derived.

Every value below is the venue's own number, read at call time. Nothing is summed across calls, marked
to a price the crate chose, or remembered.

## Contract (`SPEC.md` §6b)

```rust
// src/cex/account.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode { Isolated, Cross }

#[derive(Debug, Clone)]
pub struct PerpPosition {
    pub symbol: String,
    /// Signed: negative is short. Zero when flat, which is an answer and not
    /// an error.
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    /// `None` when flat, or when the venue reports none.
    pub liquidation_price: Option<Decimal>,
    pub margin_mode: MarginMode,
    pub leverage: u32,
    /// `Some` exactly when `margin_mode` is `Isolated`.
    pub isolated_margin: Option<Decimal>,
    /// The venue's own update time for this position, in Unix ms.
    pub as_of_ms: i64,
}

#[derive(Debug, Clone)]
pub struct MarginState {
    /// The asset these figures are in, e.g. "USDT".
    pub asset: String,
    pub margin_balance: Decimal,
    pub maint_margin: Decimal,
    pub available: Decimal,
    pub as_of_ms: i64,
}

#[derive(Debug, Clone)]
pub struct FundingPayment {
    pub symbol: String,
    pub ts_ms: i64,
    /// Signed: positive was received, negative was paid.
    pub amount: Decimal,
    pub asset: String,
    /// The venue's id for this payment, so a caller can de-duplicate.
    pub venue_ref: u64,
}

#[async_trait]
pub trait CexAccount: Send + Sync {
    async fn position(&self, symbol: &str) -> Result<PerpPosition>;
    async fn margin(&self) -> Result<MarginState>;
    /// Every payment with `ts_ms >= since_ms`, oldest first. The adapter pages
    /// through the venue's limit itself; the result is never cut short
    /// without a word.
    async fn funding_since(&self, symbol: &str, since_ms: i64) -> Result<Vec<FundingPayment>>;
    fn label(&self) -> &'static str;
}
```

Reads carry no `Provenance`. Nothing is sent, so there is no "sent or thrown away" to report. Which
account was read, testnet or production, is the adapter's `label()` and base URL.

## Implementations

| Implementation | Source |
| --- | --- |
| `BinanceFuturesAccount` | Shares `BinanceFuturesRest` with V2. Position: `GET /fapi/v3/positionRisk?symbol=`, with margin type and leverage from `GET /fapi/v1/symbolConfig?symbol=` where the position endpoint does not return them. Margin: `GET /fapi/v3/account` (`totalMarginBalance`, `totalMaintMargin`, `availableBalance`). Funding: `GET /fapi/v1/income?incomeType=FUNDING_FEE&symbol=&startTime=`, paged by time, 1,000 rows per page. Endpoint versions change. Use whichever the testnet serves and record it in the module docs |
| `CexAccountStub` | Programmable values and errors; records its calls |

In one-way mode (V2 refuses to connect otherwise), the venue reports one position per symbol, so
`position` returns one value. If the venue returns more than one row for a symbol, that is an error,
never a sum.

## Contract suite: `cex_account_contract`

- `isolated_margin.is_some() == (margin_mode == Isolated)`.
- `qty == 0` implies `liquidation_price.is_none()`.
- `funding_since` is sorted by `ts_ms`, every `ts_ms >= since_ms`, and there are no repeated
  `venue_ref`s.
- `asset` is never empty.

It runs against the stub every time, and against the testnet under the same gate as V2.

## Tests

- `wiremock`: each endpoint's parsing; funding across three pages; a flat position; an isolated and a
  cross position; two rows for one symbol giving an error.
- On the testnet, gated: open a short with V2, read it back here, and match quantity and entry price
  to the fill. Then close it with a reduce-only order and read `qty == 0`.

## Done when

The suite passes against the stub and the testnet, and `SPEC.md` has §6b and the amended §2 sentence.
