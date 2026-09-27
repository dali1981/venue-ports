//! The DEX port. See `SPEC.md` §5 — this module is the contract; do not
//! diverge from it without updating the spec first.

use crate::Provenance;
use anyhow::Result;
use async_trait::async_trait;

pub mod evm;

/// A chain-native token amount. This crate does not interpret decimals,
/// symbols, or USD value — that belongs to whatever quoted the route.
pub type ChainAmount = u128; // or a wrapper over the chain SDK's own 256-bit
                             // integer type, e.g. `alloy_primitives::U256`

/// An address on the chain in question. Left abstract here; the concrete
/// implementation for a given chain family uses that chain's own type
/// (e.g. `alloy_primitives::Address` for EVM chains, a base58 pubkey for
/// Solana-style chains).
pub type ChainAddress = Vec<u8>;

/// A route already priced by something upstream of this crate (an
/// aggregator's quote endpoint, a pool's own math, a router's simulation).
/// This crate turns it into a transaction and runs it — it never fetches
/// one itself.
#[derive(Debug, Clone)]
pub struct RouteQuote {
    pub chain_id: u64,
    pub token_in: ChainAddress,
    pub token_out: ChainAddress,
    pub amount_in: ChainAmount,
    pub expected_amount_out: ChainAmount,
    /// Opaque payload from whatever produced this quote — a route object,
    /// a serialized call, anything the adapter that consumes it knows how
    /// to turn into calldata. Never inspected outside the adapter.
    pub payload: Vec<u8>,
}

/// What the caller wants built on top of a `RouteQuote`.
#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub sender: ChainAddress,
    pub recipient: ChainAddress,
    /// The minimum acceptable output, as a literal amount. Never a
    /// percentage or basis-point tolerance recomputed at build time — the
    /// caller has already decided the number that makes this worth doing,
    /// and that is the number a revert should be measured against.
    pub min_amount_out: ChainAmount,
    /// Unix timestamp after which the built transaction must not execute.
    /// A swap without one that lands late still lands — a caller that
    /// cares when it lands must set this.
    pub deadline_unix_secs: u64,
}

/// A route bound to a request: ready to run, one way or another. What
/// `to`/`calldata`/`value` actually mean is chain-specific; this shape is
/// deliberately generic across chain families.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub to: ChainAddress,
    pub calldata: Vec<u8>,
    pub value: ChainAmount,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    /// The swap reverted. There is no output amount; there is a reason.
    Reverted {
        reason: String,
    },
    /// A transaction was sent but never reached a terminal state inside
    /// the adapter's own timeout. This is not a revert: the money's state
    /// is genuinely unknown and must be resolved out of band (a receipt
    /// poll that outlives this call, a manual check) before it is treated
    /// as anything else.
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct Realised {
    /// `None` exactly when `outcome` is not `Success`. A revert or a
    /// timeout must never be represented as a zero amount — a zero is a
    /// real, terrible price; "no price" is a different fact.
    pub amount_out: Option<ChainAmount>,
    pub outcome: Outcome,
    /// The block (or slot) the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub tx_ref: Option<Vec<u8>>, // a transaction hash / signature, chain-specific encoding
}

#[async_trait]
pub trait DexExecutor: Send + Sync {
    /// Turn a quoted route into something the network can run. May call
    /// out to whatever routing/build endpoint the venue exposes (e.g. an
    /// aggregator's "build this route into calldata" call) — this is
    /// still preparation, not execution, and nothing is sent yet.
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared>;

    /// Run the prepared swap to a terminal outcome. **Does not return
    /// until the outcome is known** — a simulated adapter returns as soon
    /// as its throwaway call answers; a live adapter signs, broadcasts,
    /// and polls until the transaction lands, reverts, or the adapter's
    /// own timeout elapses (`Outcome::TimedOut`). A caller must never have
    /// to separately "check what happened" after calling this.
    ///
    /// `at`, when given, asks the adapter to read state as of a specific
    /// block/slot instead of the latest one — used to re-run the same
    /// prepared call later, to see how far the answer has moved.
    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised>;

    /// A short, stable label for logging — e.g. `"evm-live"`,
    /// `"evm-simulated"`, `"evm-stub"`.
    fn label(&self) -> &'static str;
}
