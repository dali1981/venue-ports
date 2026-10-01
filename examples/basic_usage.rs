//! A minimal sketch of how a trading system calls into this crate.
//!
//! This crate does not quote routes, size orders, or decide anything (see
//! `SPEC.md` §2's non-goals) — a real caller would get `route`/`order`
//! below from elsewhere (a router's own quote endpoint, its own sizing
//! logic) and would replace `DexStub`/`CexStub` with `EvmSimulated`/
//! `EvmLive` and a venue's `Live` adapter once it wants a real (or
//! simulated) fill instead of a programmed one.

use std::str::FromStr;

use rust_decimal::Decimal;
use venue_ports::cex::{CexExecutor, CexStub, OrderRequest, OrderSide, OrderStateUnknown};
use venue_ports::dex::{DexExecutor, DexStub, Outcome, RouteQuote, SwapRequest};
use venue_ports::Network;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_dex_leg().await?;
    run_cex_leg().await?;
    Ok(())
}

async fn run_dex_leg() -> anyhow::Result<()> {
    let dex = DexStub::new();

    // Stands in for a route an aggregator already priced upstream.
    let route = RouteQuote {
        network: Network::evm(1),
        token_in: vec![0xA0; 20],
        token_out: vec![0xB1; 20],
        amount_in: 1_000_000,
        expected_amount_out: 950_000,
        payload: Vec::new(),
    };
    let req = SwapRequest {
        sender: vec![0xC2; 20],
        recipient: vec![0xC2; 20],
        min_amount_out: 900_000,
        deadline_unix_secs: 0,
    };

    let prepared = dex.prepare(&route, &req).await?;
    let realised = dex.execute(&prepared, None).await?;

    // The caller's only job is to react to what came back — never to
    // assume `Success`, and never to treat a revert/timeout as a price.
    match realised.outcome {
        Outcome::Success => {
            let amount_out = realised
                .amount_out
                .expect("Success always carries an amount");
            println!(
                "[{}] swap filled: {amount_out} out ({:?}, block {})",
                dex.label(),
                realised.provenance,
                realised.at
            );
        }
        Outcome::Reverted { reason } => {
            println!("[{}] swap reverted: {reason}", dex.label());
        }
        Outcome::TimedOut => {
            println!(
                "[{}] swap timed out — outcome unknown, resolve out of band",
                dex.label()
            );
        }
        Outcome::Expired => {
            println!("[{}] swap expired — it can never land", dex.label());
        }
    }

    Ok(())
}

async fn run_cex_leg() -> anyhow::Result<()> {
    let cex = CexStub::new();

    // Stands in for an order size/price a strategy already decided on.
    let req = OrderRequest {
        symbol: "SOLUSDT".to_string(),
        side: OrderSide::Buy,
        quantity: Decimal::from_str("10")?,
        quoted_price: Decimal::from_str("150.25")?,
        // A spot order: there is no position to reduce. A consumer closing
        // part of a perp position sets this to `true` (SPEC.md §6).
        reduce_only: false,
    };

    match cex.execute(&req).await {
        Ok(fill) => println!(
            "[{}] order filled: {} {} @ {} ({:?})",
            cex.label(),
            fill.filled_qty,
            req.symbol,
            fill.filled_price,
            fill.provenance
        ),
        // The one error that does not mean "nothing filled": the order may
        // have reached the venue, so the caller finds out (e.g. by reading
        // its position) before acting on this symbol again.
        Err(err) if err.downcast_ref::<OrderStateUnknown>().is_some() => {
            println!("[{}] order state unknown — resolve it: {err}", cex.label())
        }
        Err(err) => println!("[{}] order rejected, nothing filled: {err}", cex.label()),
    }

    Ok(())
}
