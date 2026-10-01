//! The liquidity port (`SPEC.md` §5b): mint, increase, decrease, collect
//! and burn on concentrated-liquidity position managers. This module is the
//! contract; do not diverge from it without updating the spec first.
//!
//! A position goes through a **position manager**, not a router, and its
//! outcomes have a different shape from a swap's: a mint returns a position
//! id, an amount of liquidity and two token amounts, and a decrease moves
//! nothing until a later collect. The port turns an action the caller has
//! already decided on into a known outcome, like the other two. It does not
//! choose ranges, compute liquidity from prices or prices from ticks, value
//! a position, or remember what it minted.

use crate::dex::{ChainAddress, ChainAmount, Outcome, Prepared};
use crate::Provenance;
use anyhow::{bail, Result};
use async_trait::async_trait;

pub mod evm;
mod stub;

pub use evm::EvmLiquidity;
pub use stub::{LiquidityCall, LiquidityStub};

/// Identifies the pool a range belongs to. It is also what decides how the
/// manager's `mint` is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolKey {
    /// Uniswap v3's NonfungiblePositionManager and its ABI-identical forks
    /// (PancakeSwap v3): the fee tier in hundredths of a basis point
    /// (500 = 0.05 %).
    Fee(u32),
    /// Aerodrome and Velodrome Slipstream: the pool's tick spacing. Their
    /// `mint` takes one more argument, `sqrtPriceX96`, which this crate
    /// always sends as zero because it never creates a pool.
    TickSpacing(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeSpec {
    pub chain_id: u64,
    /// The position manager contract, not the pool.
    pub manager: ChainAddress,
    /// In the pool's own order: token0 < token1.
    pub token0: ChainAddress,
    pub token1: ChainAddress,
    pub pool_key: PoolKey,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// A position held in a manager. On v3-style managers it is an ERC-721
/// token id, as 32 big-endian bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRef {
    pub chain_id: u64,
    pub manager: ChainAddress,
    pub id: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum LiquidityAction {
    Mint {
        range: RangeSpec,
        amount0_desired: ChainAmount,
        amount1_desired: ChainAmount,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    Increase {
        position: PositionRef,
        amount0_desired: ChainAmount,
        amount1_desired: ChainAmount,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    /// Moves `liquidity` out of the range and into the position's owed
    /// tokens. **It transfers nothing**: tokens leave only on `Collect`.
    Decrease {
        position: PositionRef,
        liquidity: u128,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    /// Transfers the owed tokens to the owner, up to the caps. Owed tokens
    /// are the principal released by earlier decreases plus the fees earned.
    Collect {
        position: PositionRef,
        amount0_max: u128,
        amount1_max: u128,
    },
    /// Destroys a position that has no liquidity and nothing owed.
    Burn { position: PositionRef },
}

impl LiquidityAction {
    /// The action's name, for errors and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            LiquidityAction::Mint { .. } => "Mint",
            LiquidityAction::Increase { .. } => "Increase",
            LiquidityAction::Decrease { .. } => "Decrease",
            LiquidityAction::Collect { .. } => "Collect",
            LiquidityAction::Burn { .. } => "Burn",
        }
    }

    pub fn chain_id(&self) -> u64 {
        match self {
            LiquidityAction::Mint { range, .. } => range.chain_id,
            LiquidityAction::Increase { position, .. }
            | LiquidityAction::Decrease { position, .. }
            | LiquidityAction::Collect { position, .. }
            | LiquidityAction::Burn { position } => position.chain_id,
        }
    }

    pub fn manager(&self) -> &ChainAddress {
        match self {
            LiquidityAction::Mint { range, .. } => &range.manager,
            LiquidityAction::Increase { position, .. }
            | LiquidityAction::Decrease { position, .. }
            | LiquidityAction::Collect { position, .. }
            | LiquidityAction::Burn { position } => &position.manager,
        }
    }

    /// The position acted on; `None` for `Mint`, whose position does not
    /// exist yet.
    pub fn position(&self) -> Option<&PositionRef> {
        match self {
            LiquidityAction::Mint { .. } => None,
            LiquidityAction::Increase { position, .. }
            | LiquidityAction::Decrease { position, .. }
            | LiquidityAction::Collect { position, .. }
            | LiquidityAction::Burn { position } => Some(position),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiquidityRequest {
    /// Signs and pays. It also receives the minted position and the collected
    /// tokens: the crate never sends either anywhere else. Never the zero
    /// address.
    pub owner: ChainAddress,
    /// Unix time after which the action must not execute. Required, as for a
    /// swap.
    pub deadline_unix_secs: u64,
}

/// What a liquidity action did. **Shape rules**, asserted for every
/// implementation by `liquidity_executor_contract`:
///
/// - `position`, `liquidity_delta`, `amount0` and `amount1` are all `Some`
///   exactly when `outcome` is `Success`. A revert or a timeout is never
///   shown as zeros.
/// - `tx_ref` is `Some` exactly when `provenance` is `Landed`.
///
/// What the amounts mean depends on the action (`SPEC.md` §5b's table):
/// tokens paid in for `Mint`/`Increase`, tokens *credited to the position's
/// owed balance* for `Decrease`, tokens transferred to the owner for
/// `Collect`, and zero for `Burn`. `liquidity_delta` is zero for `Collect`
/// and `Burn` — real values, not placeholders.
#[derive(Debug, Clone)]
pub struct LiquidityRealised {
    pub outcome: Outcome,
    /// The position acted on; for `Mint`, the new one.
    pub position: Option<PositionRef>,
    pub liquidity_delta: Option<u128>,
    pub amount0: Option<ChainAmount>,
    pub amount1: Option<ChainAmount>,
    /// The block the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub tx_ref: Option<Vec<u8>>,
}

/// Returned inside `anyhow::Error` when an action's transaction landed but
/// its outcome could not be read from the receipt: an expected event was
/// missing, or the cross-check failed. Something happened on chain, so the
/// caller must inspect `tx_ref` before it acts on this position again.
///
/// This is the one exception to the port's error rule: **an `Err` means
/// nothing was sent for the action itself, unless it is `LandedUnread`.** A
/// failed approval is a plain `Err`: it is setup, and no position changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedUnread {
    pub tx_ref: Vec<u8>,
    pub reason: String,
}

impl std::fmt::Display for LandedUnread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transaction 0x{} landed but its outcome could not be read: {}",
            hex::encode(&self.tx_ref),
            self.reason
        )
    }
}

impl std::error::Error for LandedUnread {}

#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    /// Validates the action and encodes it. It may read the chain (the
    /// position's owner and tokens) but sends nothing.
    async fn prepare(&self, action: &LiquidityAction, req: &LiquidityRequest) -> Result<Prepared>;
    /// Runs the prepared action to a terminal outcome and does not return
    /// before then, like `DexExecutor::execute`.
    ///
    /// There is no `at` parameter: a liquidity action changes state that
    /// later actions depend on, so re-running one alone at an older block
    /// means nothing. A caller who wants an older state starts the fork at
    /// that block.
    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityRealised>;
    fn label(&self) -> &'static str;
}

/// The checks every implementation makes before anything else, the stub
/// included, so a test against the stub is refused what the real adapter
/// would refuse. None of them reads the chain.
pub(crate) fn validate(action: &LiquidityAction, req: &LiquidityRequest) -> Result<()> {
    if req.owner.is_empty() || req.owner.iter().all(|b| *b == 0) {
        bail!("LiquidityRequest.owner must be set, and never the zero address");
    }
    if req.deadline_unix_secs == 0 {
        bail!("LiquidityRequest.deadline_unix_secs must be set");
    }
    let now = unix_now();
    if now >= req.deadline_unix_secs {
        bail!(
            "LiquidityRequest.deadline_unix_secs ({}) has already passed (now {now})",
            req.deadline_unix_secs
        );
    }
    match action {
        LiquidityAction::Mint {
            range,
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
        } => {
            if range.token0 >= range.token1 {
                bail!("RangeSpec.token0 must sort below token1, in the pool's own order");
            }
            if range.tick_lower >= range.tick_upper {
                bail!(
                    "RangeSpec.tick_lower ({}) must be below tick_upper ({})",
                    range.tick_lower,
                    range.tick_upper
                );
            }
            check_amounts(
                *amount0_desired,
                *amount1_desired,
                *amount0_min,
                *amount1_min,
            )
        }
        LiquidityAction::Increase {
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
            ..
        } => check_amounts(
            *amount0_desired,
            *amount1_desired,
            *amount0_min,
            *amount1_min,
        ),
        LiquidityAction::Decrease { liquidity, .. } => {
            if *liquidity == 0 {
                bail!("a Decrease must remove more than zero liquidity");
            }
            Ok(())
        }
        LiquidityAction::Collect {
            amount0_max,
            amount1_max,
            ..
        } => {
            if *amount0_max == 0 && *amount1_max == 0 {
                bail!("a Collect needs at least one cap above zero");
            }
            Ok(())
        }
        LiquidityAction::Burn { .. } => Ok(()),
    }
}

fn check_amounts(
    amount0_desired: ChainAmount,
    amount1_desired: ChainAmount,
    amount0_min: ChainAmount,
    amount1_min: ChainAmount,
) -> Result<()> {
    if amount0_desired == 0 && amount1_desired == 0 {
        bail!("the desired amounts must not both be zero");
    }
    if amount0_min > amount0_desired || amount1_min > amount1_desired {
        bail!(
            "each minimum must be at most its desired amount \
             (amount0 {amount0_min} > {amount0_desired} or amount1 {amount1_min} > {amount1_desired})"
        );
    }
    Ok(())
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range() -> RangeSpec {
        RangeSpec {
            chain_id: 1,
            manager: vec![0x11; 20],
            token0: vec![0xA0; 20],
            token1: vec![0xB0; 20],
            pool_key: PoolKey::Fee(3_000),
            tick_lower: -60,
            tick_upper: 60,
        }
    }

    fn request() -> LiquidityRequest {
        LiquidityRequest {
            owner: vec![0xCC; 20],
            deadline_unix_secs: unix_now() + 600,
        }
    }

    fn mint(range: RangeSpec, desired: (u128, u128), min: (u128, u128)) -> LiquidityAction {
        LiquidityAction::Mint {
            range,
            amount0_desired: desired.0,
            amount1_desired: desired.1,
            amount0_min: min.0,
            amount1_min: min.1,
        }
    }

    #[test]
    fn a_well_formed_mint_passes() {
        validate(&mint(range(), (10, 10), (0, 0)), &request()).unwrap();
    }

    #[test]
    fn refuses_a_zero_owner_and_a_missing_or_passed_deadline() {
        let action = mint(range(), (10, 10), (0, 0));
        let zero_owner = LiquidityRequest {
            owner: vec![0; 20],
            ..request()
        };
        assert!(validate(&action, &zero_owner).is_err());
        let no_deadline = LiquidityRequest {
            deadline_unix_secs: 0,
            ..request()
        };
        assert!(validate(&action, &no_deadline).is_err());
        let passed = LiquidityRequest {
            deadline_unix_secs: unix_now() - 1,
            ..request()
        };
        assert!(validate(&action, &passed)
            .unwrap_err()
            .to_string()
            .contains("already passed"));
    }

    #[test]
    fn refuses_a_malformed_mint() {
        let swapped = RangeSpec {
            token0: vec![0xB0; 20],
            token1: vec![0xA0; 20],
            ..range()
        };
        assert!(validate(&mint(swapped, (10, 10), (0, 0)), &request()).is_err());
        let inverted = RangeSpec {
            tick_lower: 60,
            tick_upper: -60,
            ..range()
        };
        assert!(validate(&mint(inverted, (10, 10), (0, 0)), &request()).is_err());
        assert!(validate(&mint(range(), (0, 0), (0, 0)), &request()).is_err());
        assert!(validate(&mint(range(), (10, 10), (11, 0)), &request()).is_err());
    }

    #[test]
    fn refuses_an_empty_decrease_or_collect() {
        let position = PositionRef {
            chain_id: 1,
            manager: vec![0x11; 20],
            id: vec![0; 32],
        };
        let decrease = LiquidityAction::Decrease {
            position: position.clone(),
            liquidity: 0,
            amount0_min: 0,
            amount1_min: 0,
        };
        assert!(validate(&decrease, &request()).is_err());
        let collect = LiquidityAction::Collect {
            position,
            amount0_max: 0,
            amount1_max: 0,
        };
        assert!(validate(&collect, &request()).is_err());
    }
}
