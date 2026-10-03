//! The liquidity port (`SPEC.md` §5b): one contract for every venue that
//! holds a concentrated-liquidity position — commands in, normalized events
//! out, and capabilities the caller branches on. This module is the
//! contract; do not diverge from it without updating the spec first.
//!
//! A venue is an adapter behind the port: Uniswap v3's position managers
//! ([`EvmLiquidity`]), Orca's Whirlpool ([`WhirlpoolLiquidity`]), a
//! consumer's paper model. A venue's
//! own types — its ABI, its instructions, its accounts, its events — never
//! cross the port. The caller sends a [`LiquidityCommand`], reads a
//! [`LiquidityEvent`], and branches only on
//! [`LiquidityCapabilities::deposit_guard`], to build the guard.
//!
//! The port turns a command the caller has already decided on into a known
//! outcome. It does not choose ranges, compute liquidity from prices or
//! prices from ticks (each venue computes the liquidity from a deposit in
//! token amounts), value a position, or remember what it opened.

use crate::dex::{ChainAddress, Outcome, Prepared, TxCost};
use crate::{Network, Provenance};
use anyhow::{bail, Result};
use async_trait::async_trait;

mod stub;
pub mod uniswap_v3;
pub mod whirlpool;

pub use stub::{LiquidityCall, LiquidityStub};
pub use uniswap_v3::{EvmLiquidity, ManagerAbi};
pub use whirlpool::{WhirlpoolLiquidity, WHIRLPOOL_PROGRAM};

/// What the caller asks of a position (`SPEC.md` §5b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiquidityCommand {
    /// Open a position on `range` and deposit into it.
    Open { range: Range, deposit: Deposit },
    /// Deposit more into an open position.
    Add {
        position: PositionId,
        deposit: Deposit,
    },
    /// Take `liquidity` out of the range, refusing less than `min_out` of
    /// each token.
    Remove {
        position: PositionId,
        liquidity: u128,
        min_out: TokenPair,
    },
    /// Transfer to the owner everything the position owes.
    Collect { position: PositionId },
    /// Close a position that holds no liquidity and owes nothing.
    Close { position: PositionId },
}

impl LiquidityCommand {
    /// The command's name, for errors and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            LiquidityCommand::Open { .. } => "Open",
            LiquidityCommand::Add { .. } => "Add",
            LiquidityCommand::Remove { .. } => "Remove",
            LiquidityCommand::Collect { .. } => "Collect",
            LiquidityCommand::Close { .. } => "Close",
        }
    }

    pub fn network(&self) -> Network {
        match self {
            LiquidityCommand::Open { range, .. } => range.network,
            LiquidityCommand::Add { position, .. }
            | LiquidityCommand::Remove { position, .. }
            | LiquidityCommand::Collect { position }
            | LiquidityCommand::Close { position } => position.network,
        }
    }

    /// The position acted on; `None` for `Open`, whose position does not
    /// exist yet.
    pub fn position(&self) -> Option<&PositionId> {
        match self {
            LiquidityCommand::Open { .. } => None,
            LiquidityCommand::Add { position, .. }
            | LiquidityCommand::Remove { position, .. }
            | LiquidityCommand::Collect { position }
            | LiquidityCommand::Close { position } => Some(position),
        }
    }
}

/// A range in a pool. The pool, not a manager or a program: the adapter
/// knows its venue's deployment from its construction, and reads the pool's
/// tokens and key from the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub network: Network,
    pub pool: ChainAddress,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// A deposit in token amounts: at most `max` of each, under `guard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deposit {
    pub max: TokenPair,
    pub guard: DepositGuard,
}

/// The slippage guard. A venue enforces one kind, and says which in its
/// capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepositGuard {
    /// At least this much of each token goes in (Uniswap's managers).
    MinAmounts(TokenPair),
    /// The pool's price is inside this band when the deposit runs, as a
    /// Q64.64 square root (Whirlpool).
    SqrtPriceBand {
        min_sqrt_price_x64: u128,
        max_sqrt_price_x64: u128,
    },
}

impl DepositGuard {
    pub fn kind(&self) -> DepositGuardKind {
        match self {
            DepositGuard::MinAmounts(_) => DepositGuardKind::MinAmounts,
            DepositGuard::SqrtPriceBand { .. } => DepositGuardKind::SqrtPriceBand,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositGuardKind {
    MinAmounts,
    SqrtPriceBand,
}

/// Opaque to the caller: an ERC-721 id under a manager (32 big-endian
/// bytes), a position mint under a program.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PositionId {
    pub network: Network,
    pub bytes: Vec<u8>,
}

/// Two token amounts, in the pool's own token order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenPair {
    pub token0: u128,
    pub token1: u128,
}

impl TokenPair {
    pub fn new(token0: u128, token1: u128) -> Self {
        Self { token0, token1 }
    }

    pub fn is_zero(&self) -> bool {
        self.token0 == 0 && self.token1 == 0
    }

    pub fn saturating_add(self, other: TokenPair) -> TokenPair {
        TokenPair::new(
            self.token0.saturating_add(other.token0),
            self.token1.saturating_add(other.token1),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityRequest {
    /// Signs and pays. It also receives the position and every token the
    /// position transfers: the crate never sends either anywhere else.
    /// Never the zero address.
    pub owner: ChainAddress,
    /// Unix time after which the command must not execute. Required, as for
    /// a swap.
    pub deadline_unix_secs: u64,
}

/// What a command did, as facts and never as a venue's semantics. A
/// position's token flow is the sum of `paid`, less the sum of
/// `transferred`; the fees it earned are the sum of `transferred`, less the
/// sum of `released`. The caller computes both the same way for every venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiquidityEvent {
    Opened {
        position: PositionId,
        liquidity: u128,
        paid: TokenPair,
    },
    Added {
        liquidity: u128,
        paid: TokenPair,
    },
    /// `released`: the principal taken out of the range. `transferred`:
    /// what reached the owner in this transaction.
    Removed {
        liquidity: u128,
        released: TokenPair,
        transferred: TokenPair,
    },
    Collected {
        transferred: TokenPair,
    },
    Closed,
}

impl LiquidityEvent {
    /// The event's name, for errors and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            LiquidityEvent::Opened { .. } => "Opened",
            LiquidityEvent::Added { .. } => "Added",
            LiquidityEvent::Removed { .. } => "Removed",
            LiquidityEvent::Collected { .. } => "Collected",
            LiquidityEvent::Closed => "Closed",
        }
    }

    /// What reached the owner in this transaction.
    pub fn transferred(&self) -> TokenPair {
        match self {
            LiquidityEvent::Removed { transferred, .. }
            | LiquidityEvent::Collected { transferred } => *transferred,
            _ => TokenPair::default(),
        }
    }
}

/// One per command executed. **Shape rules**, asserted for every venue by
/// `liquidity_executor_contract`: `event` is `Some` exactly when `outcome`
/// is `Success`; `tx_ref` is `Some` exactly when a transaction was sent (to
/// the chain, `Landed`, or to a fork, `Simulated`);
/// `cost` is set whenever something ran, a revert included.
#[derive(Debug, Clone)]
pub struct LiquidityReport {
    pub outcome: Outcome,
    pub event: Option<LiquidityEvent>,
    pub cost: TxCost,
    /// The block or slot the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    pub tx_ref: Option<Vec<u8>>,
}

/// Returned inside `anyhow::Error` when a command's transaction landed but
/// its outcome could not be read: an expected event was missing, or the
/// cross-check failed. Something happened on chain, so the caller must
/// inspect `tx_ref` before it acts on this position again.
///
/// This is the one exception to the port's error rule: **an `Err` means
/// nothing was sent for the command itself, unless it is `LandedUnread`.** A
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

/// What a venue does that the caller may need to know. The caller branches
/// on `deposit_guard` alone, to build the guard; the other three explain
/// the events' and costs' figures, and no accounting needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityCapabilities {
    /// The guard the venue enforces on a deposit.
    pub deposit_guard: DepositGuardKind,
    /// A Remove transfers the principal at once. Otherwise it stays owed
    /// until Collect.
    pub remove_transfers: bool,
    /// Opening a range may create accounts whose rent the owner cannot
    /// close (`NativeCost.spent`).
    pub open_may_spend_rent: bool,
    /// Close returns a deposit (a negative `NativeCost.deposit`).
    pub close_returns_deposit: bool,
}

impl LiquidityCapabilities {
    /// Uniswap v3's position managers and their forks.
    pub const UNISWAP_V3: LiquidityCapabilities = LiquidityCapabilities {
        deposit_guard: DepositGuardKind::MinAmounts,
        remove_transfers: false,
        open_may_spend_rent: false,
        close_returns_deposit: false,
    };

    /// Orca's Whirlpool.
    pub const WHIRLPOOL: LiquidityCapabilities = LiquidityCapabilities {
        deposit_guard: DepositGuardKind::SqrtPriceBand,
        remove_transfers: true,
        open_may_spend_rent: true,
        close_returns_deposit: true,
    };
}

#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    fn capabilities(&self) -> LiquidityCapabilities;
    /// Validates the command and encodes it. It may read the chain (the
    /// pool, the position) but sends nothing. A command the position cannot
    /// take is refused here.
    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared>;
    /// Runs the prepared command to a terminal outcome and does not return
    /// before then, like `DexExecutor::execute`.
    ///
    /// There is no `at` parameter: a liquidity command changes state that
    /// later commands depend on, so re-running one alone at an older block
    /// means nothing. A caller who wants an older state starts the fork at
    /// that block.
    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport>;
    fn label(&self) -> &'static str;
}

/// The checks every venue makes before anything else, a stub or a paper
/// model included, so each is refused what a real venue would refuse. None
/// of them reads the chain: what the position can take (its liquidity, what
/// it owes, who holds it) is each venue's to read.
pub fn check_command(
    cmd: &LiquidityCommand,
    req: &LiquidityRequest,
    capabilities: &LiquidityCapabilities,
) -> Result<()> {
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
    match cmd {
        LiquidityCommand::Open { range, deposit } => {
            if range.tick_lower >= range.tick_upper {
                bail!(
                    "Range.tick_lower ({}) must be below tick_upper ({})",
                    range.tick_lower,
                    range.tick_upper
                );
            }
            check_deposit(deposit, capabilities)
        }
        LiquidityCommand::Add { deposit, .. } => check_deposit(deposit, capabilities),
        LiquidityCommand::Remove { liquidity, .. } => {
            if *liquidity == 0 {
                bail!("a Remove must take out more than zero liquidity");
            }
            Ok(())
        }
        LiquidityCommand::Collect { .. } | LiquidityCommand::Close { .. } => Ok(()),
    }
}

fn check_deposit(deposit: &Deposit, capabilities: &LiquidityCapabilities) -> Result<()> {
    if deposit.max.is_zero() {
        bail!("a deposit's maxima must not both be zero");
    }
    if deposit.guard.kind() != capabilities.deposit_guard {
        bail!(
            "this venue enforces a {:?} guard, not {:?}: build the guard its capabilities name",
            capabilities.deposit_guard,
            deposit.guard.kind()
        );
    }
    match deposit.guard {
        DepositGuard::MinAmounts(min) => {
            if min.token0 > deposit.max.token0 || min.token1 > deposit.max.token1 {
                bail!(
                    "each minimum must be at most its maximum (token0 {} > {} or token1 {} > {})",
                    min.token0,
                    deposit.max.token0,
                    min.token1,
                    deposit.max.token1
                );
            }
        }
        DepositGuard::SqrtPriceBand {
            min_sqrt_price_x64,
            max_sqrt_price_x64,
        } => {
            if min_sqrt_price_x64 >= max_sqrt_price_x64 {
                bail!(
                    "the sqrt-price band's minimum ({min_sqrt_price_x64}) must be below its \
                     maximum ({max_sqrt_price_x64})"
                );
            }
        }
    }
    Ok(())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range() -> Range {
        Range {
            network: Network::evm(1),
            pool: vec![0x22; 20],
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

    fn open(range: Range, max: (u128, u128), min: (u128, u128)) -> LiquidityCommand {
        LiquidityCommand::Open {
            range,
            deposit: Deposit {
                max: TokenPair::new(max.0, max.1),
                guard: DepositGuard::MinAmounts(TokenPair::new(min.0, min.1)),
            },
        }
    }

    const CAPS: LiquidityCapabilities = LiquidityCapabilities::UNISWAP_V3;

    #[test]
    fn a_well_formed_open_passes() {
        check_command(&open(range(), (10, 10), (0, 0)), &request(), &CAPS).unwrap();
    }

    #[test]
    fn refuses_a_zero_owner_and_a_missing_or_passed_deadline() {
        let cmd = open(range(), (10, 10), (0, 0));
        let zero_owner = LiquidityRequest {
            owner: vec![0; 20],
            ..request()
        };
        assert!(check_command(&cmd, &zero_owner, &CAPS).is_err());
        let no_deadline = LiquidityRequest {
            deadline_unix_secs: 0,
            ..request()
        };
        assert!(check_command(&cmd, &no_deadline, &CAPS).is_err());
        let passed = LiquidityRequest {
            deadline_unix_secs: unix_now() - 1,
            ..request()
        };
        assert!(check_command(&cmd, &passed, &CAPS)
            .unwrap_err()
            .to_string()
            .contains("already passed"));
    }

    #[test]
    fn refuses_a_malformed_open() {
        let inverted = Range {
            tick_lower: 60,
            tick_upper: -60,
            ..range()
        };
        assert!(check_command(&open(inverted, (10, 10), (0, 0)), &request(), &CAPS).is_err());
        assert!(check_command(&open(range(), (0, 0), (0, 0)), &request(), &CAPS).is_err());
        assert!(check_command(&open(range(), (10, 10), (11, 0)), &request(), &CAPS).is_err());
    }

    #[test]
    fn refuses_the_guard_the_venue_does_not_enforce() {
        let banded = LiquidityCommand::Open {
            range: range(),
            deposit: Deposit {
                max: TokenPair::new(10, 10),
                guard: DepositGuard::SqrtPriceBand {
                    min_sqrt_price_x64: 1,
                    max_sqrt_price_x64: 2,
                },
            },
        };
        let err = check_command(&banded, &request(), &CAPS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MinAmounts"), "{err}");
        check_command(&banded, &request(), &LiquidityCapabilities::WHIRLPOOL).unwrap();
        let inverted = LiquidityCommand::Open {
            range: range(),
            deposit: Deposit {
                max: TokenPair::new(10, 10),
                guard: DepositGuard::SqrtPriceBand {
                    min_sqrt_price_x64: 2,
                    max_sqrt_price_x64: 2,
                },
            },
        };
        assert!(check_command(&inverted, &request(), &LiquidityCapabilities::WHIRLPOOL).is_err());
    }

    #[test]
    fn refuses_an_empty_remove() {
        let remove = LiquidityCommand::Remove {
            position: PositionId {
                network: Network::evm(1),
                bytes: vec![0; 32],
            },
            liquidity: 0,
            min_out: TokenPair::default(),
        };
        assert!(check_command(&remove, &request(), &CAPS).is_err());
    }
}
