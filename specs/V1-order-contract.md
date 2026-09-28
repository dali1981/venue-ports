# V1 — the order contract: reduce-only, and "this may have filled"

Status: proposed. Needs nothing external. About 1 day.

## Why

**Reduce-only.** A consumer shrinking a derivatives position sends an order sized from its own view
of that position. If that view is stale, an order meant to close a short can open a long. Venues that
have positions will refuse any part of an order that would increase or flip the position, but only if
the order asks them to. `OrderRequest` has no way to ask.

**An error that hides a fill.** Today an `Err` from `CexExecutor::execute` can mean either "the venue
refused this" or "we lost track after sending it". The second case can hide a real fill. The caller
cannot tell the two apart, and treating a fill as a refusal leaves a position nobody knows about. A DEX
adapter already has a value for this state (`Outcome::TimedOut`), and the CEX port needs one too.

## Change (`SPEC.md` §6)

```rust
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: Decimal,
    pub quoted_price: Decimal,
    /// The venue must refuse any part of this order that would increase or
    /// flip the position instead of reducing it. A venue with no positions
    /// (spot) must reject `true` with an error before sending, never ignore
    /// it: an ignored reduce-only is an unguarded order.
    pub reduce_only: bool,
}

/// Returned (inside `anyhow::Error`, found with `downcast_ref`) when an
/// order may have reached the venue but its outcome could not be read: the
/// placing call's response was lost, and the status query that should
/// follow it also failed. The venue may have filled some, all or none of
/// it. The caller must find out before it acts on this symbol again, for
/// example by reading its position.
#[derive(Debug, Clone)]
pub struct OrderStateUnknown {
    pub symbol: String,
    /// The id this crate gave the order, which the venue can be asked about.
    pub client_order_id: String,
    /// `Some` if the venue acknowledged the order before contact was lost.
    pub order_ref: Option<u64>,
}
impl std::error::Error for OrderStateUnknown {}
```

The contract gains one sentence: **an `Err` from `execute` means nothing filled, unless it is an
`OrderStateUnknown`.** Every `Live` adapter must keep to it. After a venue accepts an order, a lost
response, a failed status query, or a fill whose details cannot be read all become
`OrderStateUnknown`. None of them may surface as a plain error.

**Alternative considered:** a separate futures-only request type. It was rejected because the trait and
the stub would split in two for one boolean, and the spot adapters' rejection of `true` is itself a
contract assertion worth keeping.

## Per implementation

| Implementation | Change |
| --- | --- |
| `CexStub` | Records `reduce_only`. It gains `program_state_unknown(client_order_id)`. It also gains a reduce-only mode: given a signed position set by the test, it rejects any reduce-only order that would increase or flip it. Tests that care about reduce-only therefore get the venue's behaviour without the venue. Nobody has checked what Binance does with a reduce-only order larger than the position: it may reject it, or fill up to the position and expire the rest. The stub rejects until V2's testnet run shows which, and then copies that |
| `BinanceLive` (spot) | Returns an error for `reduce_only: true` before sending anything |
| `BybitLive` (spot, `category=spot`) | The same |
| `BinanceFuturesLive` (V2) | Sends `reduceOnly=true` |

## Contract suite (`src/testkit/contract.rs`)

- `cex_executor_contract` builds its fixture with `reduce_only: false`, which changes no behaviour.
- A new `cex_spot_rejects_reduce_only(executor)` runs against every spot adapter: `reduce_only: true`
  gives an error, and no request reaches the mock server.
- `CexStub` in reduce-only mode: with a position of −5, a reduce-only buy of 3 fills and a
  reduce-only sell of 1 is rejected. A reduce-only buy of 8 is rejected until V2 records the venue's
  real behaviour.

## Done when

The above passes, `examples/basic_usage.rs` sets the field, and `SPEC.md` §6 carries both types and
the one-sentence error rule.
