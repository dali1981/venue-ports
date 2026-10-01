//! A position's whole life through the liquidity port (`SPEC.md` §5b):
//! mint, increase, decrease, collect, burn.
//!
//! This crate does not choose the range, size the position, or remember
//! what it minted (§2, §5b's non-goals): the caller decided the range and
//! amounts below, and keeps the `PositionRef` each step hands back. Against
//! `LiquidityStub` every outcome is programmed first; swap in `EvmLiquidity`
//! over a fork sender (`EvmSender::fork`) for a Simulated run, or over a
//! signing sender for a Live one — the calls do not change.

use venue_ports::dex::Outcome;
use venue_ports::liquidity::{
    LandedUnread, LiquidityAction, LiquidityExecutor, LiquidityRealised, LiquidityRequest,
    LiquidityStub, PoolKey, PositionRef, RangeSpec,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = vec![0xC3; 20];
    let range = RangeSpec {
        chain_id: 8453,
        manager: manager.clone(),
        token0: vec![0xA0; 20],
        token1: vec![0xB1; 20],
        pool_key: PoolKey::Fee(500),
        tick_lower: -1_000,
        tick_upper: 1_000,
    };
    let request = LiquidityRequest {
        owner: vec![0xD4; 20],
        deadline_unix_secs: unix_now() + 600,
    };

    // What a real manager would report, programmed up front: the stub
    // never invents liquidity or amounts.
    let stub = LiquidityStub::new();
    let position = PositionRef {
        chain_id: 8453,
        manager,
        id: token_id(1_234),
    };
    stub.program_success(position.clone(), 50_000, 1_000_000, 2_000_000, 100);
    stub.program_success(position.clone(), 10_000, 200_000, 400_000, 101);
    stub.program_success(position.clone(), 60_000, 1_199_999, 2_399_999, 102);
    stub.program_success(position.clone(), 0, 1_200_310, 2_400_020, 103);
    stub.program_success(position, 0, 0, 0, 104);

    let minted = step(
        &stub,
        LiquidityAction::Mint {
            range,
            amount0_desired: 1_000_000,
            amount1_desired: 2_000_000,
            amount0_min: 990_000,
            amount1_min: 1_980_000,
        },
        &request,
    )
    .await?;
    let Some(position) = minted.position.clone() else {
        // A revert or timeout: there is no position to go on with.
        return Ok(());
    };
    let mut liquidity = minted.liquidity_delta.unwrap_or(0);

    let increased = step(
        &stub,
        LiquidityAction::Increase {
            position: position.clone(),
            amount0_desired: 200_000,
            amount1_desired: 400_000,
            amount0_min: 0,
            amount1_min: 0,
        },
        &request,
    )
    .await?;
    liquidity += increased.liquidity_delta.unwrap_or(0);

    // Decrease moves the liquidity into the position's owed tokens and
    // transfers nothing; collect is what pays out, principal plus fees.
    step(
        &stub,
        LiquidityAction::Decrease {
            position: position.clone(),
            liquidity,
            amount0_min: 0,
            amount1_min: 0,
        },
        &request,
    )
    .await?;
    step(
        &stub,
        LiquidityAction::Collect {
            position: position.clone(),
            amount0_max: u128::MAX,
            amount1_max: u128::MAX,
        },
        &request,
    )
    .await?;
    step(&stub, LiquidityAction::Burn { position }, &request).await?;
    Ok(())
}

/// Runs one action and reacts to what came back — never assuming
/// `Success`, never reading a revert or a timeout as zeros.
async fn step(
    executor: &dyn LiquidityExecutor,
    action: LiquidityAction,
    request: &LiquidityRequest,
) -> anyhow::Result<LiquidityRealised> {
    let kind = action.kind();
    let prepared = executor.prepare(&action, request).await?;
    let realised = match executor.execute(&prepared).await {
        Ok(realised) => realised,
        Err(err) if err.downcast_ref::<LandedUnread>().is_some() => {
            // Something happened on chain that could not be read: inspect
            // the transaction before touching this position again.
            println!("[{}] {kind}: landed but unread — {err}", executor.label());
            return Err(err);
        }
        // Any other error means nothing was sent for the action.
        Err(err) => return Err(err),
    };
    match &realised.outcome {
        Outcome::Success => println!(
            "[{}] {kind}: liquidity {:?}, amounts {:?} / {:?} (block {}, {:?})",
            executor.label(),
            realised.liquidity_delta,
            realised.amount0,
            realised.amount1,
            realised.at,
            realised.provenance
        ),
        Outcome::Reverted { reason } => {
            println!("[{}] {kind}: reverted — {reason}", executor.label())
        }
        Outcome::TimedOut => println!(
            "[{}] {kind}: timed out — its fate is unknown; resolve it before anything else",
            executor.label()
        ),
        Outcome::Expired => println!(
            "[{}] {kind}: expired — it can never land",
            executor.label()
        ),
    }
    Ok(realised)
}

fn token_id(id: u64) -> Vec<u8> {
    let mut bytes = vec![0; 24];
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
