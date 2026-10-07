# V6 — balance reads, resolving an unknown order, and a fork never reads as a landing

Status: proposed, by a consumer that runs its strategy against a fork and the Binance spot testnet and must
reconcile its books to the unit against what each venue holds, and find out what became of an order whose
adapter gave up waiting (arb-searcher's M10c, task F5). Needs nothing external to build; the anvil tests and the
testnet run are gated. About 3 days.

## Why

**Balances.** The crate has no way to read what a wallet or an exchange account holds. `EvmRpc::balance` reads
`latest` only, `erc20::balance_of` takes a block but is not a port, and the only account read is a perp's
(`CexAccount`, V4). A consumer reconciling its books against the venue needs the figure the venue reports, at a
block it names, as an exact integer or an exact decimal, and a stub to test against.

**An unknown order has no way back.** Two adapters give up honestly: `EvmLive` ends a swap `Outcome::TimedOut`
and `BinanceLive` ends an order `OrderStateUnknown`. Neither can be asked again. `EvmLive` removes what it
knows about the swap before sending and never restores it, so the hash in `Realised.tx_ref` cannot be turned
into a result later. `BinanceLive`'s status query is private (`order::track`). The consumer is left to treat the
swap or the order as unknown until a restart.

**A fork run reads as a landing.** `EvmSender::connect` is `Landed` whatever node it talks to. A consumer that
signs with a key on an anvil fork, to rehearse what it will send with, writes rows that read as mainnet trades.

## The line this draws in `SPEC.md` §2

None. Reading what a venue reports about an account, or about an order the caller already placed, at the moment
it is asked, is not tracking (§2, §6b). Nothing here is kept between calls except what is needed to resolve a
swap that was left unknown (`EvmLive`) and when a lost order request was signed (`BinanceLive`); neither is a
balance, a position or a total.

## Contract

### Balances (`SPEC.md` §6c)

```rust
// src/balance/mod.rs — the BalanceReader port: one trait per kind of account
#[async_trait]
pub trait EvmBalanceReader: Send + Sync {
    /// `address`'s native balance in wei, as the node reports it at `block`.
    async fn native(&self, address: Address, block: BlockTag) -> Result<ChainAmount>;
    /// `holder`'s `balanceOf` of `token` at `block`.
    async fn token(&self, token: Address, holder: Address, block: BlockTag) -> Result<ChainAmount>;
    fn label(&self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotBalance { pub asset: String, pub free: Decimal, pub locked: Decimal }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotAccountBalances {
    /// Only assets the account holds some of: an asset that is absent is held at zero.
    pub balances: Vec<SpotBalance>,
    /// The venue's own time of the account's last change (`updateTime`), in Unix ms.
    pub update_time_ms: Option<u64>,
}

#[async_trait]
pub trait SpotBalanceReader: Send + Sync {
    async fn balances(&self) -> Result<SpotAccountBalances>;
    fn label(&self) -> &'static str;
}
```

- An amount above `u128` is an error naming the token and holder, never truncated (`ChainAmount` is `u128`).
- A balance read at a past block needs a node that serves that block's state: an archive node, or a fork that
  holds the history. A node that cannot is an `Err`, never the latest value.
- `locked` is the funds an open order holds. They are still the account's.
- Every `Err` is a read that failed; nothing was sent.

| Implementation | Source |
| --- | --- |
| `EvmBalances` | `eth_getBalance` and `balanceOf` through an `EvmRpc`, at the block named. `EvmRpc` gains `balance_at(address, block)`; `balance` is `balance_at(…, Latest)` |
| `BinanceRest`, `BinanceLive` (spot) | `GET /api/v3/account?omitZeroBalances=true`, through the signed client |
| `EvmBalanceStub`, `SpotBalanceStub` | Programmable values and errors; each records its calls. The EVM stub answers from a history: a value set at block `b` holds until a later block sets another, and a read before the first value is an error, never zero |

### Resolving a swap (`SPEC.md` §5)

```rust
// src/dex/mod.rs
/// What became of a swap whose adapter gave up waiting ([`Outcome::TimedOut`]), as the chain says now.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Not decided: the node still knows the transaction, or its nonce is unused and another node may yet
    /// broadcast it. Ask again.
    Pending,
    /// It ended: mined, a revert included. A success carries `amount_in` and `amount_out` read as a sent
    /// swap's are (what the pool took is what is booked).
    Done(Realised),
    /// Replaced or dropped, and did not land.
    Gone,
}

// src/dex/evm/live.rs
impl EvmLive {
    /// `tx_hash` is the hash an `execute` of this adapter ended `TimedOut` with (`Realised.tx_ref`).
    pub async fn resolve(&self, tx_hash: B256) -> Result<Resolution>;
}

// src/evm/tx.rs
/// The error `EvmSender::resolve` returns when the transaction was replaced or dropped. Found with `downcast_ref`.
pub struct TxReplacedOrDropped { pub tx_hash: B256, pub nonce: u64, pub address: Address }
```

- `EvmLive` keeps the swap's context by hash when a send ends `TimedOut`, and drops it at `Done` and `Gone`. A
  hash this adapter did not time out (another process's, or one it never saw) is an `Err`.
- **`Gone` only for replaced or dropped.** `EvmSender::resolve` returns `Err` for that and for an RPC failure.
  The first is now a `TxReplacedOrDropped`, with the message it always had; any other `Err` is a failed read, the
  latch is intact, and `EvmLive::resolve` returns it as an `Err`.
- The transfer-log decoders `execute` reads a landed swap's amounts with are shared, so `Done` and `execute`
  cannot disagree.
- Resolve through `EvmLive`, not the sender: `EvmSender::resolve` clears the latch and hands the outcome to
  whoever called it. If the sender no longer holds the hash, `EvmLive::resolve` is an `Err`.

### Resolving an order (`SPEC.md` §6)

```rust
// src/cex/orders.rs
#[derive(Debug, Clone)]
pub enum OrderState {
    /// Ended `FILLED`: the whole fill.
    Filled(CexFill),
    /// Ended with part of it filled and the rest not (`EXPIRED`, `CANCELED`, `EXPIRED_IN_MATCH`, for example a
    /// market order that ran out of book). What `execute` returns as a partial `CexFill`.
    PartlyFilled(CexFill),
    /// Ended with nothing filled: the venue accepted the order and it ended in `status`.
    Rejected { order_ref: u64, status: String },
    /// The venue has no such order, and cannot accept the request that placed it any more: nothing filled.
    NotFound,
    /// Not settled: still working, or the venue does not know it yet and could still accept the lost request.
    /// Ask again.
    Open,
}

#[async_trait]
pub trait CexOrders: Send + Sync {
    /// The state of the order this adapter placed under `client_order_id` (the id an `OrderStateUnknown` names).
    async fn order_state(&self, symbol: &str, client_order_id: &str) -> Result<OrderState>;
    fn label(&self) -> &'static str;
}
```

- **One read, no waiting.** `order_state` asks the venue once, with the same query `execute` follows an order
  with (`GET /api/v3/order?origClientOrderId=`), and reads the trade lines of a fill as `execute` does. The caller
  sets the schedule.
- **`Err` is a read that failed** (the query, or the trade lines of a fill, could not be read): ask again. It is
  never `NotFound`.
- **`NotFound` is conclusive only after `recvWindow` and the clock's error bound** have passed since the lost
  request was signed. `BinanceLive` remembers when it signed each placing request whose answer it lost, until
  that order's state is read. An answer of "no such order" before then is `Open`. For an id it holds no lost
  request for (placed with an answer, or not this adapter's), "no such order" is `NotFound` at once.
- No caller-chosen client order id exists: the id is the adapter's own, so the resolver cannot make a resend
  look like a first send. Binance does not guard a resend of a filled order's id.
- `CexStub` implements `CexOrders`: programmable per id, recording its calls. `BinanceLive` implements it;
  `BinanceFuturesLive` and `BybitLive` do not.

### A fork never reads as a landing (`SPEC.md` §5, the EVM sender)

`EvmSender::connect` reads `web3_clientVersion` once, after the chain id and before registering. A node whose
version starts with `anvil` makes the sender `Simulated`: `provenance()`, and so every outcome an adapter
reports through it, says it. Any other version is `Landed`. A node that answers with an error object (it does
not implement the method) is not anvil. A node that cannot be asked at all is an error, since the sender cannot
say what it sends to. `ensure_balance` still refuses to write a balance on a signing sender, anvil included.

## Findings recorded here

- **`PriorityBid::AbovePolicyPerGas` is refused, by name, by the signing sender's adapter.** `EvmSender` has no
  priority-fee parameter: `send_and_confirm(to, calldata, value)` prices the transaction by its `FeePolicy`
  alone, for a signing sender and a fork sender alike. `EvmLive::prepare` calls `policy_only("EvmLive")`, which
  refuses any bid above zero. Nothing is sent and nothing is ignored. It is left refused; a pinning test says so.

## Tests

- `wiremock`: each reader's parsing, on bodies copied from Binance's documentation (each marked
  `TODO(R2)` until the recorded bodies replace it); `EvmLive::resolve` for pending, done (a success and a
  revert), gone, an RPC error with the entry and latch intact, and a hash it never timed out; every
  `OrderState`; `NotFound` held back as `Open` until the window has passed.
- Contract suites in `testkit`: `evm_balance_reader_contract`, `spot_balance_reader_contract`,
  `cex_orders_contract`, each run against its stub.
- Gated on `EVM_ANVIL_RPC_URL` (a no-op when unset): a signing sender is `Simulated` on anvil; `EvmBalances`
  reads a past block's balance after the balance has moved; a timed-out `EvmLive` swap is `Pending`, then `Done`
  with its amounts once a block is mined.

## Done when

The suites pass against the stubs, the `wiremock` cases pass, the anvil tests pass on a node, and `SPEC.md` has
§5's `Resolution` and `EvmLive::resolve`, §5's sender rule, §6's `CexOrders`, and §6c.
