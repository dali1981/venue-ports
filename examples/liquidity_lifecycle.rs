//! A position's whole life through the liquidity port (`SPEC.md` §5b):
//! open, add, remove, collect, close.
//!
//! This crate does not choose the range, size the position, or remember
//! what it opened (§2, §5b's non-goals): the caller decided the range and
//! amounts below, and keeps the `PositionId` the `Opened` event hands back.
//! The caller names no venue and branches on one capability only, to build
//! the deposit's guard; the token flow and the fees come from the events,
//! the same way on every venue. Against `LiquidityStub` every outcome is
//! programmed first; swap in `EvmLiquidity` over a fork sender
//! (`EvmSender::fork`) for a Simulated run, or over a signing sender for a
//! Live one — the calls do not change.

use venue_ports::dex::Outcome;
use venue_ports::liquidity::{
    Deposit, DepositGuard, DepositGuardKind, LandedUnread, LiquidityCapabilities,
    LiquidityCommand, LiquidityEvent, LiquidityExecutor, LiquidityRequest, LiquidityStub,
    PositionId, Range, TokenPair,
};
use venue_ports::Network;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = Network::evm(8453);
    let range = Range {
        network: base,
        pool: vec![0xC3; 20],
        tick_lower: -1_000,
        tick_upper: 1_000,
    };
    let request = LiquidityRequest {
        owner: vec![0xD4; 20],
        deadline_unix_secs: unix_now() + 600,
    };

    // What a real manager would report, programmed up front: the stub
    // never invents liquidity or amounts.
    let stub = LiquidityStub::new(LiquidityCapabilities::UNISWAP_V3);
    let position = PositionId {
        network: base,
        bytes: token_id(1_234),
    };
    stub.program_event(
        LiquidityEvent::Opened {
            position: position.clone(),
            liquidity: 50_000,
            paid: TokenPair::new(1_000_000, 2_000_000),
        },
        100,
    );
    stub.program_event(
        LiquidityEvent::Added {
            liquidity: 10_000,
            paid: TokenPair::new(200_000, 400_000),
        },
        101,
    );
    stub.program_event(
        LiquidityEvent::Removed {
            liquidity: 60_000,
            released: TokenPair::new(1_199_999, 2_399_999),
            transferred: TokenPair::default(),
        },
        102,
    );
    stub.program_event(
        LiquidityEvent::Collected {
            transferred: TokenPair::new(1_200_310, 2_400_020),
        },
        103,
    );
    stub.program_event(LiquidityEvent::Closed, 104);

    // The one branch a caller makes: the guard the venue enforces.
    let deposit = |max: TokenPair| Deposit {
        max,
        guard: match stub.capabilities().deposit_guard {
            DepositGuardKind::MinAmounts => DepositGuard::MinAmounts(TokenPair::new(
                max.token0 / 100 * 99,
                max.token1 / 100 * 99,
            )),
            DepositGuardKind::SqrtPriceBand => DepositGuard::SqrtPriceBand {
                min_sqrt_price_x64: 1 << 63,
                max_sqrt_price_x64: 1 << 65,
            },
        },
    };

    let mut events = Vec::new();
    let Some(opened) = step(
        &stub,
        LiquidityCommand::Open {
            range,
            deposit: deposit(TokenPair::new(1_000_000, 2_000_000)),
        },
        &request,
    )
    .await?
    else {
        // A revert or timeout: there is no position to go on with.
        return Ok(());
    };
    let LiquidityEvent::Opened {
        position,
        mut liquidity,
        ..
    } = opened.clone()
    else {
        unreachable!("an Open answers Opened");
    };
    events.push(opened);

    let Some(added) = step(
        &stub,
        LiquidityCommand::Add {
            position: position.clone(),
            deposit: deposit(TokenPair::new(200_000, 400_000)),
        },
        &request,
    )
    .await?
    else {
        return Ok(());
    };
    if let LiquidityEvent::Added { liquidity: more, .. } = added {
        liquidity += more;
    }
    events.push(added);

    for cmd in [
        LiquidityCommand::Remove {
            position: position.clone(),
            liquidity,
            min_out: TokenPair::default(),
        },
        LiquidityCommand::Collect {
            position: position.clone(),
        },
        LiquidityCommand::Close { position },
    ] {
        let Some(event) = step(&stub, cmd, &request).await? else {
            return Ok(());
        };
        events.push(event);
    }

    // The same accounting on every venue: what reached the owner, less
    // what it released, is what the position earned.
    let mut paid = TokenPair::default();
    let mut released = TokenPair::default();
    let mut transferred = TokenPair::default();
    for event in &events {
        match event {
            LiquidityEvent::Opened { paid: p, .. } | LiquidityEvent::Added { paid: p, .. } => {
                paid = paid.saturating_add(*p)
            }
            LiquidityEvent::Removed { released: r, .. } => released = released.saturating_add(*r),
            _ => {}
        }
        transferred = transferred.saturating_add(event.transferred());
    }
    println!(
        "paid {paid:?}, got back {transferred:?}, fees earned ({}, {})",
        transferred.token0 - released.token0,
        transferred.token1 - released.token1
    );
    Ok(())
}

/// Runs one command and reacts to what came back — never assuming
/// `Success`, never reading a revert or a timeout as zeros. `None` when it
/// did not succeed.
async fn step(
    executor: &dyn LiquidityExecutor,
    cmd: LiquidityCommand,
    request: &LiquidityRequest,
) -> anyhow::Result<Option<LiquidityEvent>> {
    let kind = cmd.kind();
    let prepared = executor.prepare(&cmd, request).await?;
    let report = match executor.execute(&prepared).await {
        Ok(report) => report,
        Err(err) if err.downcast_ref::<LandedUnread>().is_some() => {
            // Something happened on chain that could not be read: inspect
            // the transaction before touching this position again.
            println!("[{}] {kind}: landed but unread — {err}", executor.label());
            return Err(err);
        }
        // Any other error means nothing was sent for the command.
        Err(err) => return Err(err),
    };
    match &report.outcome {
        Outcome::Success => println!(
            "[{}] {kind}: {:?} (block {}, {:?}, cost {:?})",
            executor.label(),
            report.event,
            report.at,
            report.provenance,
            report.cost.native()
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
    Ok(report.event)
}

fn token_id(id: u64) -> Vec<u8> {
    let mut bytes = vec![0; 32];
    bytes[24..].copy_from_slice(&id.to_be_bytes());
    bytes
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
