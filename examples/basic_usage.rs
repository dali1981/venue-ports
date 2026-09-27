//! A minimal sketch of how a trading system calls into this crate.
//!
//! This crate does not quote routes, size orders, or decide anything (see
//! `SPEC.md` §2's non-goals) — a real caller would get `route`/`order`
//! below from elsewhere (a router's own quote endpoint, its own sizing
//! logic) and would replace `EvmStub`/`CexStub` with `EvmSimulated`/
//! `EvmLive` and a venue's `Live` adapter once it wants a real (or
//! simulated) fill instead of a programmed one.

use std::str::FromStr;

use rust_decimal::Decimal;
use venue_ports::cex::{CexExecutor, CexStub, OrderRequest, OrderSide};
use venue_ports::dex::evm::EvmStub;
use venue_ports::dex::{DexExecutor, Outcome, RouteQuote, SwapRequest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_dex_leg().await?;
    run_cex_leg().await?;
    Ok(())
}

async fn run_dex_leg() -> anyhow::Result<()> {
    let dex = EvmStub::new();

    // Stands in for a route an aggregator already priced upstream.
    let route = RouteQuote {
        chain_id: 1,
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
        Err(err) => println!("[{}] order rejected: {err}", cex.label()),
    }

    Ok(())
}
