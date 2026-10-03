//! `EvmLiquidity` — Uniswap v3's position managers as a liquidity venue,
//! over an `Arc<EvmSender>` (`SPEC.md` §5b, `IMPLEMENTATION_PLAN.md` Phases
//! 13 and 15).
//!
//! **One implementation, two senders.** A position's life is a sequence of
//! commands, each depending on the state the last one left, and an
//! `eth_call` discards that state — so unlike a swap there is no separate
//! dry-run simulator. Given a signing sender this is Live; given a fork
//! sender ([`EvmSender::fork`]) it runs the same transactions on an anvil
//! fork as an impersonated owner, and is Simulated. Every line of encoding
//! and decoding is shared, which is what the contract suite proves.
//!
//! **The venue's deployment comes from construction**: the manager's
//! address and its ABI. A command names the pool; the pool's tokens and key
//! (its fee tier, or Slipstream's tick spacing) are read from the pool, and
//! a pool whose factory is not the manager's is refused, since the manager
//! would mint into a different pool.
//!
//! **Reading the outcome.** Amounts come from the manager's own events
//! (`log.address == manager`): the pool emits `Mint` and `Collect` events of
//! its own with other signatures, and those are ignored. They are then
//! **cross-checked** against the ERC-20 `Transfer` logs of the same receipt —
//! for `Open` and `Add` the owner must have sent exactly what was paid, for
//! `Collect` it must have received exactly what was transferred. A mismatch
//! (a fee-on-transfer token, a wrong decoder) or a missing event is
//! [`LandedUnread`], never a guessed number. A `Remove` transfers nothing
//! here: the principal stays owed until `Collect`.
//!
//! **Approvals** go to the manager, for exactly the deposit's maximum of
//! each token, never `U256::MAX`. Any unspent part stays behind, capped at
//! that maximum, and the next deposit approves exactly what it needs.

use crate::dex::{EvmCall, EvmCost, Outcome, Prepared, TxCost};
use crate::evm::erc20;
use crate::evm::rpc::{address_from_slice, BlockTag};
use crate::evm::{prepared_key, EvmSender, RpcLog, TxOutcome};
use crate::liquidity::uniswap_v3::abi::{manager, pool, slipstream, uniswap_v3};
use crate::liquidity::{
    check_command, unix_now, Deposit, DepositGuard, LandedUnread, LiquidityCapabilities,
    LiquidityCommand, LiquidityEvent, LiquidityExecutor, LiquidityReport, LiquidityRequest,
    PositionId, TokenPair,
};
use crate::{Network, Provenance};
use alloy_primitives::aliases::{I24, U160, U24};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Which position manager ABI the deployment speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerAbi {
    /// Uniswap v3's `NonfungiblePositionManager`: the pool key is the fee
    /// tier.
    UniswapV3,
    /// PancakeSwap v3's: ABI-identical to Uniswap v3's.
    PancakeSwapV3,
    /// Aerodrome and Velodrome Slipstream: the pool key is the tick
    /// spacing, and `mint` ends with `sqrtPriceX96`, always sent as zero
    /// (this crate never creates a pool).
    Slipstream,
}

/// A pool's key, as its manager's `mint` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolKey {
    Fee(u32),
    TickSpacing(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Add,
    Remove,
    Collect,
    Close,
}

/// Everything `execute()` needs that isn't in `Prepared`, stashed by
/// `prepare()` and consumed exactly once.
#[derive(Debug, Clone)]
struct PendingCommand {
    kind: Kind,
    owner: Address,
    deadline_unix_secs: u64,
    /// The pool's tokens, when this command moves them: `Open`, `Add` and
    /// `Collect`.
    tokens: Option<(Address, Address)>,
    /// What `Open` and `Add` approve, per token: the deposit's maxima.
    max: (u128, u128),
    /// The position's id; `None` for `Open`, which learns it from the
    /// receipt.
    id: Option<U256>,
}

/// What `positions(id)` says of a position.
#[derive(Debug, Clone, Copy)]
struct PositionState {
    tokens: (Address, Address),
    liquidity: u128,
    owed: (u128, u128),
}

pub struct EvmLiquidity {
    sender: Arc<EvmSender>,
    manager: Address,
    abi: ManagerAbi,
    pending: Mutex<HashMap<B256, PendingCommand>>,
}

impl EvmLiquidity {
    pub fn new(sender: Arc<EvmSender>, manager: Address, abi: ManagerAbi) -> Self {
        Self {
            sender,
            manager,
            abi,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn sender(&self) -> &Arc<EvmSender> {
        &self.sender
    }

    pub fn manager(&self) -> Address {
        self.manager
    }

    fn network(&self) -> Network {
        Network::evm(self.sender.chain_id())
    }

    async fn read(&self, to: Address, data: &[u8], what: &str) -> Result<Vec<u8>> {
        self.sender
            .rpc()
            .eth_call(to, data, None, BlockTag::Latest, None)
            .await
            .with_context(|| format!("reading {what} on {to}"))
    }

    /// `(token0, token1, key)` from the pool, refused unless the pool comes
    /// from the manager's own factory.
    async fn pool(&self, pool_address: Address) -> Result<(Address, Address, PoolKey)> {
        let address = |data: Vec<u8>, what: &str| -> Result<Address> {
            let word = data
                .get(..32)
                .ok_or_else(|| anyhow!("{what} on {pool_address} returned {} bytes", data.len()))?;
            Ok(Address::from_slice(&word[12..]))
        };
        let token0 = address(
            self.read(pool_address, &pool::token0Call {}.abi_encode(), "token0()")
                .await?,
            "token0()",
        )?;
        let token1 = address(
            self.read(pool_address, &pool::token1Call {}.abi_encode(), "token1()")
                .await?,
            "token1()",
        )?;
        let key = match self.abi {
            ManagerAbi::UniswapV3 | ManagerAbi::PancakeSwapV3 => {
                let data = self
                    .read(pool_address, &pool::feeCall {}.abi_encode(), "fee()")
                    .await?;
                let fee = pool::feeCall::abi_decode_returns(&data)
                    .with_context(|| format!("decoding fee() on {pool_address}"))?;
                PoolKey::Fee(fee.to::<u32>())
            }
            ManagerAbi::Slipstream => {
                let data = self
                    .read(
                        pool_address,
                        &pool::tickSpacingCall {}.abi_encode(),
                        "tickSpacing()",
                    )
                    .await?;
                let spacing = pool::tickSpacingCall::abi_decode_returns(&data)
                    .with_context(|| format!("decoding tickSpacing() on {pool_address}"))?;
                PoolKey::TickSpacing(spacing.as_i32())
            }
        };
        let pool_factory = address(
            self.read(
                pool_address,
                &pool::factoryCall {}.abi_encode(),
                "factory()",
            )
            .await?,
            "factory()",
        )?;
        let manager_factory = address(
            self.read(
                self.manager,
                &manager::factoryCall {}.abi_encode(),
                "factory()",
            )
            .await?,
            "factory()",
        )?;
        if pool_factory != manager_factory {
            bail!(
                "pool {pool_address} comes from factory {pool_factory}, not from the manager's \
                 ({manager_factory}): the manager would open the position in another pool"
            );
        }
        Ok((token0, token1, key))
    }

    /// `ownerOf(id)` on the manager must be `owner`.
    async fn check_owner(&self, id: U256, owner: Address) -> Result<()> {
        let data = self
            .read(
                self.manager,
                &manager::ownerOfCall { tokenId: id }.abi_encode(),
                &format!("ownerOf({id})"),
            )
            .await?;
        let holder = manager::ownerOfCall::abi_decode_returns(&data)
            .with_context(|| format!("decoding ownerOf({id}) on {}", self.manager))?;
        if holder != owner {
            bail!(
                "position {id} on {} is owned by {holder}, not by {owner}",
                self.manager
            );
        }
        Ok(())
    }

    /// `positions(id)`: the tokens (words 2 and 3), the liquidity (word 7)
    /// and the tokens owed (words 10 and 11), the same on every supported
    /// manager.
    async fn position_state(&self, id: U256) -> Result<PositionState> {
        let data = self
            .read(
                self.manager,
                &manager::positionsCall { tokenId: id }.abi_encode(),
                &format!("positions({id})"),
            )
            .await?;
        let word = |i: usize| {
            data.get(i * 32..(i + 1) * 32)
                .ok_or_else(|| anyhow!("positions({id}) returned only {} bytes", data.len()))
        };
        let uint = |i: usize| -> Result<u128> {
            let value = U256::from_be_slice(word(i)?);
            u128::try_from(value).map_err(|_| anyhow!("positions({id}) word {i} is {value}"))
        };
        Ok(PositionState {
            tokens: (
                Address::from_slice(&word(2)?[12..]),
                Address::from_slice(&word(3)?[12..]),
            ),
            liquidity: uint(7)?,
            owed: (uint(10)?, uint(11)?),
        })
    }

    fn unsettled(
        &self,
        outcome: Outcome,
        cost: EvmCost,
        at: u64,
        tx_hash: B256,
    ) -> LiquidityReport {
        LiquidityReport {
            outcome,
            event: None,
            cost: TxCost::Evm(cost),
            at,
            provenance: self.sender.provenance(),
            tx_ref: Some(tx_hash.as_slice().to_vec()),
        }
    }

    fn position_id(&self, position: &PositionId) -> Result<U256> {
        if position.network != self.network() {
            bail!(
                "position is on network {}, but this EvmLiquidity's sender is on {}",
                position.network,
                self.network()
            );
        }
        if position.bytes.len() != 32 {
            bail!(
                "a position under a manager is 32 big-endian bytes, got {}",
                position.bytes.len()
            );
        }
        Ok(U256::from_be_slice(&position.bytes))
    }

    /// Reads a successful command's event from its receipt, or explains why
    /// it cannot (which the caller turns into `LandedUnread`).
    fn read_success(
        &self,
        ctx: &PendingCommand,
        logs: &[RpcLog],
    ) -> std::result::Result<LiquidityEvent, String> {
        let manager = self.manager;
        let (id, liquidity, amount0, amount1) = match ctx.kind {
            Kind::Open => {
                let id = find_nft_transfer(logs, manager, Address::ZERO, ctx.owner)
                    .ok_or("no ERC-721 Transfer of a new position to the owner")?;
                let event = find_event::<manager::IncreaseLiquidity>(logs, manager, id)
                    .ok_or("no IncreaseLiquidity event for the new position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Add => {
                let id = ctx.id.expect("Add always has an id");
                let event = find_event::<manager::IncreaseLiquidity>(logs, manager, id)
                    .ok_or("no IncreaseLiquidity event for the position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Remove => {
                let id = ctx.id.expect("Remove always has an id");
                let event = find_event::<manager::DecreaseLiquidity>(logs, manager, id)
                    .ok_or("no DecreaseLiquidity event for the position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Collect => {
                let id = ctx.id.expect("Collect always has an id");
                let event = find_event::<manager::Collect>(logs, manager, id)
                    .ok_or("no Collect event for the position")?;
                (id, 0, event.amount0, event.amount1)
            }
            Kind::Close => {
                let id = ctx.id.expect("Close always has an id");
                find_nft_transfer(logs, manager, ctx.owner, Address::ZERO)
                    .filter(|burnt| *burnt == id)
                    .ok_or("no ERC-721 Transfer of the position to the zero address")?;
                return Ok(LiquidityEvent::Closed);
            }
        };

        if let Some((token0, token1)) = ctx.tokens {
            let transfers = erc20::transfers(logs);
            let moved = |token: Address| -> U256 {
                transfers
                    .iter()
                    .filter(|t| t.token == token)
                    .filter(|t| match ctx.kind {
                        Kind::Collect => t.to == ctx.owner,
                        _ => t.from == ctx.owner,
                    })
                    .map(|t| t.value)
                    .fold(U256::ZERO, |sum, v| sum.saturating_add(v))
            };
            let direction = if ctx.kind == Kind::Collect {
                "received"
            } else {
                "sent"
            };
            for (name, token, reported) in
                [("token0", token0, amount0), ("token1", token1, amount1)]
            {
                // The pool transfers nothing for a zero amount.
                if reported.is_zero() {
                    continue;
                }
                let actual = moved(token);
                if actual != reported {
                    return Err(format!(
                        "the manager reports {reported} of {name} ({token}) but the owner {direction} \
                         {actual} per the receipt's Transfer logs — a fee-on-transfer token, or a \
                         wrong decoder"
                    ));
                }
            }
        }

        let narrow = |name: &str, v: U256| -> std::result::Result<u128, String> {
            u128::try_from(v).map_err(|_| format!("{name} {v} does not fit in u128"))
        };
        let amounts = TokenPair::new(narrow("amount0", amount0)?, narrow("amount1", amount1)?);
        Ok(match ctx.kind {
            Kind::Open => LiquidityEvent::Opened {
                position: PositionId {
                    network: self.network(),
                    bytes: id.to_be_bytes::<32>().to_vec(),
                },
                liquidity,
                paid: amounts,
            },
            Kind::Add => LiquidityEvent::Added {
                liquidity,
                paid: amounts,
            },
            // The principal stays owed until Collect.
            Kind::Remove => LiquidityEvent::Removed {
                liquidity,
                released: amounts,
                transferred: TokenPair::default(),
            },
            Kind::Collect => LiquidityEvent::Collected {
                transferred: amounts,
            },
            Kind::Close => unreachable!("returned above"),
        })
    }
}

/// The token id of an ERC-721 `Transfer(from → to, id)` emitted by
/// `manager`.
fn find_nft_transfer(
    logs: &[RpcLog],
    manager: Address,
    from: Address,
    to: Address,
) -> Option<U256> {
    logs.iter()
        .filter(|log| log.address == manager && log.topics.len() == 4)
        .filter_map(|log| {
            manager::Transfer::decode_raw_log(log.topics.iter().copied(), &log.data).ok()
        })
        .find(|t| t.from == from && t.to == to)
        .map(|t| t.tokenId)
}

/// The first `E` emitted by `manager` whose indexed token id is `id`.
fn find_event<E: SolEvent>(logs: &[RpcLog], manager: Address, id: U256) -> Option<E> {
    let id_topic = B256::from(id);
    logs.iter()
        .filter(|log| {
            log.address == manager
                && log.topics.first() == Some(&E::SIGNATURE_HASH)
                && log.topics.get(1) == Some(&id_topic)
        })
        .find_map(|log| E::decode_raw_log(log.topics.iter().copied(), &log.data).ok())
}

fn int24(name: &str, value: i32) -> Result<I24> {
    I24::try_from(value).map_err(|_| anyhow!("{name} {value} does not fit in an int24"))
}

/// A deposit's maxima and minima. `check_command` has already refused any
/// guard but `MinAmounts`.
fn min_amounts(deposit: &Deposit) -> TokenPair {
    match deposit.guard {
        DepositGuard::MinAmounts(min) => min,
        DepositGuard::SqrtPriceBand { .. } => {
            unreachable!("check_command refuses a sqrt-price band on a MinAmounts venue")
        }
    }
}

#[async_trait]
impl LiquidityExecutor for EvmLiquidity {
    fn capabilities(&self) -> LiquidityCapabilities {
        LiquidityCapabilities::UNISWAP_V3
    }

    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared> {
        check_command(cmd, req, &self.capabilities())?;
        let owner = address_from_slice(&req.owner).context("LiquidityRequest.owner")?;
        if owner != self.sender.address() {
            bail!(
                "LiquidityRequest.owner ({owner}) is not this EvmLiquidity's sending address ({}) \
                 — it can only act as its own sender",
                self.sender.address()
            );
        }
        if cmd.network() != self.network() {
            bail!(
                "the command is for network {}, but this EvmLiquidity's sender is on {}",
                cmd.network(),
                self.network()
            );
        }
        let deadline = U256::from(req.deadline_unix_secs);
        let pending = |kind, tokens, max, id| PendingCommand {
            kind,
            owner,
            deadline_unix_secs: req.deadline_unix_secs,
            tokens,
            max,
            id,
        };

        let (calldata, pending) = match cmd {
            LiquidityCommand::Open { range, deposit } => {
                let pool_address = address_from_slice(&range.pool).context("Range.pool")?;
                let (token0, token1, key) = self.pool(pool_address).await?;
                let tick_lower = int24("tick_lower", range.tick_lower)?;
                let tick_upper = int24("tick_upper", range.tick_upper)?;
                let min = min_amounts(deposit);
                let calldata = match key {
                    PoolKey::Fee(fee) => uniswap_v3::mintCall {
                        params: uniswap_v3::MintParams {
                            token0,
                            token1,
                            fee: U24::try_from(fee)
                                .map_err(|_| anyhow!("fee {fee} does not fit in a uint24"))?,
                            tickLower: tick_lower,
                            tickUpper: tick_upper,
                            amount0Desired: U256::from(deposit.max.token0),
                            amount1Desired: U256::from(deposit.max.token1),
                            amount0Min: U256::from(min.token0),
                            amount1Min: U256::from(min.token1),
                            recipient: owner,
                            deadline,
                        },
                    }
                    .abi_encode(),
                    PoolKey::TickSpacing(spacing) => slipstream::mintCall {
                        params: slipstream::MintParams {
                            token0,
                            token1,
                            tickSpacing: int24("tick spacing", spacing)?,
                            tickLower: tick_lower,
                            tickUpper: tick_upper,
                            amount0Desired: U256::from(deposit.max.token0),
                            amount1Desired: U256::from(deposit.max.token1),
                            amount0Min: U256::from(min.token0),
                            amount1Min: U256::from(min.token1),
                            recipient: owner,
                            deadline,
                            // Never creates a pool.
                            sqrtPriceX96: U160::ZERO,
                        },
                    }
                    .abi_encode(),
                };
                (
                    calldata,
                    pending(
                        Kind::Open,
                        Some((token0, token1)),
                        (deposit.max.token0, deposit.max.token1),
                        None,
                    ),
                )
            }
            LiquidityCommand::Add { position, deposit } => {
                let id = self.position_id(position)?;
                self.check_owner(id, owner).await?;
                let state = self.position_state(id).await?;
                let min = min_amounts(deposit);
                let calldata = manager::increaseLiquidityCall {
                    params: manager::IncreaseLiquidityParams {
                        tokenId: id,
                        amount0Desired: U256::from(deposit.max.token0),
                        amount1Desired: U256::from(deposit.max.token1),
                        amount0Min: U256::from(min.token0),
                        amount1Min: U256::from(min.token1),
                        deadline,
                    },
                }
                .abi_encode();
                (
                    calldata,
                    pending(
                        Kind::Add,
                        Some(state.tokens),
                        (deposit.max.token0, deposit.max.token1),
                        Some(id),
                    ),
                )
            }
            LiquidityCommand::Remove {
                position,
                liquidity,
                min_out,
            } => {
                let id = self.position_id(position)?;
                self.check_owner(id, owner).await?;
                let state = self.position_state(id).await?;
                if *liquidity > state.liquidity {
                    bail!(
                        "a Remove of {liquidity} is above position {id}'s liquidity, {}",
                        state.liquidity
                    );
                }
                let calldata = manager::decreaseLiquidityCall {
                    params: manager::DecreaseLiquidityParams {
                        tokenId: id,
                        liquidity: *liquidity,
                        amount0Min: U256::from(min_out.token0),
                        amount1Min: U256::from(min_out.token1),
                        deadline,
                    },
                }
                .abi_encode();
                (calldata, pending(Kind::Remove, None, (0, 0), Some(id)))
            }
            LiquidityCommand::Collect { position } => {
                let id = self.position_id(position)?;
                self.check_owner(id, owner).await?;
                let state = self.position_state(id).await?;
                let calldata = manager::collectCall {
                    params: manager::CollectParams {
                        tokenId: id,
                        recipient: owner,
                        amount0Max: u128::MAX,
                        amount1Max: u128::MAX,
                    },
                }
                .abi_encode();
                (
                    calldata,
                    pending(Kind::Collect, Some(state.tokens), (0, 0), Some(id)),
                )
            }
            LiquidityCommand::Close { position } => {
                let id = self.position_id(position)?;
                self.check_owner(id, owner).await?;
                let state = self.position_state(id).await?;
                if state.liquidity > 0 || state.owed != (0, 0) {
                    bail!(
                        "a Close on position {id}, which holds liquidity ({}) or owes tokens \
                         ({}, {}): remove and collect first",
                        state.liquidity,
                        state.owed.0,
                        state.owed.1
                    );
                }
                (
                    manager::burnCall { tokenId: id }.abi_encode(),
                    pending(Kind::Close, None, (0, 0), Some(id)),
                )
            }
        };

        let call = EvmCall {
            to: self.manager.as_slice().to_vec(),
            calldata,
            value: 0,
        };
        self.pending
            .lock()
            .unwrap()
            .insert(prepared_key(&call), pending);
        Ok(Prepared::Evm(call))
    }

    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport> {
        let call = prepared.evm_call()?;
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&prepared_key(call))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this EvmLiquidity instance did not \
                     produce, or already consumed"
                )
            })?;

        let now = unix_now();
        if now >= ctx.deadline_unix_secs {
            bail!(
                "LiquidityRequest.deadline_unix_secs ({}) has already passed (now {now}) — \
                 refusing to send",
                ctx.deadline_unix_secs
            );
        }

        if matches!(ctx.kind, Kind::Open | Kind::Add) {
            let (token0, token1) = ctx.tokens.expect("Open and Add know their tokens");
            for (token, max) in [(token0, ctx.max.0), (token1, ctx.max.1)] {
                if max == 0 {
                    continue;
                }
                let amount = U256::from(max);
                self.sender
                    .ensure_balance(token, self.sender.address(), amount)
                    .await?;
                self.sender
                    .ensure_allowance(token, self.manager, amount)
                    .await?;
            }
        }

        match self
            .sender
            .send_and_confirm(self.manager, call.calldata.clone(), U256::ZERO)
            .await?
        {
            TxOutcome::Success {
                block,
                tx_hash,
                logs,
                cost,
            } => {
                let event = self
                    .read_success(&ctx, &logs)
                    .map_err(|reason| LandedUnread {
                        tx_ref: tx_hash.as_slice().to_vec(),
                        reason,
                    })?;
                Ok(LiquidityReport {
                    outcome: Outcome::Success,
                    event: Some(event),
                    cost: TxCost::Evm(cost),
                    at: block,
                    provenance: self.sender.provenance(),
                    tx_ref: Some(tx_hash.as_slice().to_vec()),
                })
            }
            TxOutcome::Reverted {
                block,
                tx_hash,
                reason,
                cost,
            } => Ok(self.unsettled(Outcome::Reverted { reason }, cost, block, tx_hash)),
            TxOutcome::TimedOut { tx_hash } => {
                let at = self.sender.rpc().block_number().await.unwrap_or(0);
                Ok(self.unsettled(Outcome::TimedOut, EvmCost::default(), at, tx_hash))
            }
        }
    }

    fn label(&self) -> &'static str {
        match self.sender.provenance() {
            Provenance::Landed => "evm-liquidity-live",
            Provenance::Simulated => "evm-liquidity-fork",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::erc20::{transfer_topic, ALLOWANCE_SELECTOR, BALANCE_OF_SELECTOR};
    use crate::evm::rpc::{encode_error_string, format_u256, hex_data, pad_address};
    use crate::evm::tx::tests::{connect, mount_receipt, mount_send_plumbing, SEPOLIA};
    use crate::evm::tx::unique_test_signer;
    use crate::evm::{EvmRpc, FeePolicy, Signer};
    use crate::liquidity::Range;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MANAGER: Address = Address::repeat_byte(0x11);
    const POOL: Address = Address::repeat_byte(0x22);
    const FACTORY: Address = Address::repeat_byte(0x33);
    const TOKEN0: Address = Address::repeat_byte(0xA0);
    const TOKEN1: Address = Address::repeat_byte(0xB0);
    const ID: u64 = 7;

    fn topic(address: Address) -> String {
        hex_data(&pad_address(address))
    }

    fn nft_transfer(from: Address, to: Address, id: u64) -> Value {
        json!({
            "address": MANAGER.to_string(),
            "topics": [transfer_topic().to_string(), topic(from), topic(to),
                       format_u256(U256::from(id))],
            "data": "0x",
        })
    }

    fn erc20_transfer(token: Address, from: Address, to: Address, value: U256) -> Value {
        json!({
            "address": token.to_string(),
            "topics": [transfer_topic().to_string(), topic(from), topic(to)],
            "data": format_u256(value),
        })
    }

    /// `IncreaseLiquidity`/`DecreaseLiquidity(id, liquidity, amount0, amount1)`
    /// or `Collect(id, recipient, amount0, amount1)` from the manager.
    fn manager_event(signature_hash: B256, id: u64, words: [U256; 3]) -> Value {
        let data: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes::<32>()).collect();
        json!({
            "address": MANAGER.to_string(),
            "topics": [signature_hash.to_string(), format_u256(U256::from(id))],
            "data": hex_data(&data),
        })
    }

    /// What the mock chain holds: who owns position `ID`, its liquidity and
    /// what it owes, and the factory the pool says it came from.
    struct Chain {
        owner_of: Option<Address>,
        liquidity: u128,
        owed: (u128, u128),
        pool_factory: Address,
    }

    impl Default for Chain {
        fn default() -> Self {
            Self {
                owner_of: None,
                liquidity: 1_000,
                owed: (0, 0),
                pool_factory: FACTORY,
            }
        }
    }

    /// A mock node for a signing sender whose owner holds and has approved
    /// everything, whose pool is `TOKEN0`/`TOKEN1` at 0.3 % from `FACTORY`,
    /// and whose one transaction lands with `logs` (or reverts with
    /// `revert`).
    async fn node(chain: Chain, logs: Value, revert: Option<&str>) -> MockServer {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let owner_of = chain
            .owner_of
            .map(|o| format_u256(U256::from_be_slice(&pad_address(o))));
        let reverts = revert.is_some();
        let revert = revert.map(|r| hex_data(&encode_error_string(r)));
        let address_word = |a: Address| format_u256(U256::from_be_slice(&pad_address(a)));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let data = body["params"][0]["data"].as_str().unwrap_or("").to_string();
                let to: Address = body["params"][0]["to"].as_str().unwrap().parse().unwrap();
                let starts = |selector: [u8; 4]| data.starts_with(&hex_data(&selector));
                let result = if starts(BALANCE_OF_SELECTOR) || starts(ALLOWANCE_SELECTOR) {
                    format_u256(U256::MAX)
                } else if starts(manager::ownerOfCall::SELECTOR) {
                    owner_of.clone().unwrap_or_else(|| format_u256(U256::ZERO))
                } else if starts(manager::positionsCall::SELECTOR) {
                    let mut words = vec![0u8; 12 * 32];
                    words[2 * 32..3 * 32].copy_from_slice(&pad_address(TOKEN0));
                    words[3 * 32..4 * 32].copy_from_slice(&pad_address(TOKEN1));
                    for (i, v) in [(7, chain.liquidity), (10, chain.owed.0), (11, chain.owed.1)] {
                        words[i * 32..(i + 1) * 32]
                            .copy_from_slice(&U256::from(v).to_be_bytes::<32>());
                    }
                    hex_data(&words)
                } else if starts(pool::factoryCall::SELECTOR) {
                    address_word(if to == POOL {
                        chain.pool_factory
                    } else {
                        FACTORY
                    })
                } else if starts(pool::token0Call::SELECTOR) {
                    address_word(TOKEN0)
                } else if starts(pool::token1Call::SELECTOR) {
                    address_word(TOKEN1)
                } else if starts(pool::feeCall::SELECTOR) {
                    format_u256(U256::from(3_000u64))
                } else if let Some(revert) = &revert {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1,
                        "error": { "code": 3, "message": "execution reverted", "data": revert },
                    }));
                } else {
                    "0x".to_string()
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(&server)
            .await;
        let status = if reverts { "0x0" } else { "0x1" };
        mount_receipt(
            &server,
            json!({
                "status": status, "blockNumber": "0x2a", "logs": logs,
                "gasUsed": "0x30d40", "effectiveGasPrice": "0x3b9aca00",
            }),
        )
        .await;
        server
    }

    /// Whether the node was asked to broadcast anything.
    async fn sent_anything(server: &MockServer) -> bool {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|req| {
                let body: Value = req.body_json().unwrap_or_default();
                body["method"] == "eth_sendRawTransaction"
            })
    }

    fn request(owner: Address) -> LiquidityRequest {
        LiquidityRequest {
            owner: owner.as_slice().to_vec(),
            deadline_unix_secs: unix_now() + 600,
        }
    }

    fn open() -> LiquidityCommand {
        LiquidityCommand::Open {
            range: Range {
                network: Network::evm(SEPOLIA),
                pool: POOL.as_slice().to_vec(),
                tick_lower: -600,
                tick_upper: 600,
            },
            deposit: Deposit {
                max: TokenPair::new(100, 100),
                guard: DepositGuard::MinAmounts(TokenPair::default()),
            },
        }
    }

    fn position() -> PositionId {
        PositionId {
            network: Network::evm(SEPOLIA),
            bytes: U256::from(ID).to_be_bytes::<32>().to_vec(),
        }
    }

    /// An adapter over a signing sender for `signer`, whose address the
    /// mocked logs were built with.
    async fn liquidity_on(server: &MockServer, signer: Signer) -> EvmLiquidity {
        EvmLiquidity::new(
            EvmSender::connect(
                EvmRpc::new(server.uri()),
                signer,
                SEPOLIA,
                FeePolicy::default(),
            )
            .await
            .unwrap(),
            MANAGER,
            ManagerAbi::UniswapV3,
        )
    }

    #[tokio::test]
    async fn an_open_reads_the_new_position_from_the_managers_events() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain::default(),
            json!([
                erc20_transfer(TOKEN0, owner, POOL, U256::from(50u64)),
                erc20_transfer(TOKEN1, owner, POOL, U256::from(60u64)),
                // The pool's own Mint event, a different signature from a
                // different address: ignored.
                { "address": POOL.to_string(),
                  "topics": [B256::repeat_byte(0x99).to_string(), topic(MANAGER)],
                  "data": format_u256(U256::from(1u64)) },
                nft_transfer(Address::ZERO, owner, ID),
                manager_event(
                    manager::IncreaseLiquidity::SIGNATURE_HASH,
                    ID,
                    [U256::from(1_000u64), U256::from(50u64), U256::from(60u64)]
                ),
            ]),
            None,
        )
        .await;

        let liquidity = liquidity_on(&server, signer).await;
        let prepared = liquidity.prepare(&open(), &request(owner)).await.unwrap();
        let report = liquidity.execute(&prepared).await.unwrap();

        assert!(matches!(report.outcome, Outcome::Success));
        assert_eq!(
            report.event,
            Some(LiquidityEvent::Opened {
                position: position(),
                liquidity: 1_000,
                paid: TokenPair::new(50, 60),
            })
        );
        assert_eq!(
            report.cost,
            TxCost::Evm(EvmCost {
                gas_used: Some(200_000),
                effective_gas_price_wei: Some(1_000_000_000),
                l1_fee_wei: None,
            })
        );
        assert_eq!(report.at, 42);
        assert_eq!(report.provenance, Provenance::Landed);
        assert!(report.tx_ref.is_some());
        assert_eq!(liquidity.label(), "evm-liquidity-live");
        assert_eq!(liquidity.capabilities(), LiquidityCapabilities::UNISWAP_V3);
    }

    #[tokio::test]
    async fn an_open_on_a_pool_from_another_factory_is_refused_before_sending() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                pool_factory: Address::repeat_byte(0x44),
                ..Chain::default()
            },
            json!([]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let err = liquidity
            .prepare(&open(), &request(owner))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("another pool"), "{err}");
        assert!(!sent_anything(&server).await);
    }

    #[tokio::test]
    async fn a_fee_on_transfer_token_is_landed_unread() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain::default(),
            json!([
                erc20_transfer(TOKEN0, owner, POOL, U256::from(49u64)),
                erc20_transfer(TOKEN1, owner, POOL, U256::from(60u64)),
                nft_transfer(Address::ZERO, owner, ID),
                manager_event(
                    manager::IncreaseLiquidity::SIGNATURE_HASH,
                    ID,
                    [U256::from(1_000u64), U256::from(50u64), U256::from(60u64)]
                ),
            ]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let prepared = liquidity.prepare(&open(), &request(owner)).await.unwrap();
        let err = liquidity.execute(&prepared).await.unwrap_err();

        let unread = err.downcast_ref::<LandedUnread>().expect("LandedUnread");
        assert!(unread.reason.contains("50"), "{}", unread.reason);
        assert!(unread.reason.contains("49"), "{}", unread.reason);
        assert_eq!(unread.tx_ref.len(), 32);
    }

    #[tokio::test]
    async fn a_missing_event_is_landed_unread() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain::default(),
            json!([nft_transfer(Address::ZERO, owner, ID)]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let prepared = liquidity.prepare(&open(), &request(owner)).await.unwrap();
        let err = liquidity.execute(&prepared).await.unwrap_err();

        let unread = err.downcast_ref::<LandedUnread>().expect("LandedUnread");
        assert!(unread.reason.contains("IncreaseLiquidity"));
    }

    #[tokio::test]
    async fn an_amount_above_u128_is_landed_unread_never_truncated() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let huge = U256::from(u128::MAX) + U256::from(1u64);
        let server = node(
            Chain::default(),
            json!([
                erc20_transfer(TOKEN0, owner, POOL, huge),
                nft_transfer(Address::ZERO, owner, ID),
                manager_event(
                    manager::IncreaseLiquidity::SIGNATURE_HASH,
                    ID,
                    [U256::from(1u64), huge, U256::ZERO]
                ),
            ]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let prepared = liquidity.prepare(&open(), &request(owner)).await.unwrap();
        let err = liquidity.execute(&prepared).await.unwrap_err();

        let unread = err.downcast_ref::<LandedUnread>().expect("LandedUnread");
        assert!(unread.reason.contains("does not fit in u128"));
    }

    #[tokio::test]
    async fn a_remove_releases_the_principal_and_transfers_nothing() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(owner),
                ..Chain::default()
            },
            json!([manager_event(
                manager::DecreaseLiquidity::SIGNATURE_HASH,
                ID,
                [U256::from(1_000u64), U256::from(49u64), U256::from(59u64)]
            )]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let remove = LiquidityCommand::Remove {
            position: position(),
            liquidity: 1_000,
            min_out: TokenPair::default(),
        };
        let prepared = liquidity.prepare(&remove, &request(owner)).await.unwrap();
        let report = liquidity.execute(&prepared).await.unwrap();

        assert_eq!(
            report.event,
            Some(LiquidityEvent::Removed {
                liquidity: 1_000,
                released: TokenPair::new(49, 59),
                transferred: TokenPair::default(),
            })
        );
    }

    #[tokio::test]
    async fn a_collect_is_cross_checked_against_what_the_owner_received() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(owner),
                ..Chain::default()
            },
            json!([
                erc20_transfer(TOKEN0, POOL, owner, U256::from(75u64)),
                erc20_transfer(TOKEN1, POOL, owner, U256::from(90u64)),
                manager_event(
                    manager::Collect::SIGNATURE_HASH,
                    ID,
                    [
                        U256::from_be_slice(&pad_address(owner)),
                        U256::from(75u64),
                        U256::from(90u64)
                    ]
                ),
            ]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let collect = LiquidityCommand::Collect {
            position: position(),
        };
        let prepared = liquidity.prepare(&collect, &request(owner)).await.unwrap();
        let report = liquidity.execute(&prepared).await.unwrap();

        assert_eq!(
            report.event,
            Some(LiquidityEvent::Collected {
                transferred: TokenPair::new(75, 90),
            })
        );
    }

    #[tokio::test]
    async fn a_close_moves_nothing() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(owner),
                liquidity: 0,
                ..Chain::default()
            },
            json!([nft_transfer(owner, Address::ZERO, ID)]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let close = LiquidityCommand::Close {
            position: position(),
        };
        let prepared = liquidity.prepare(&close, &request(owner)).await.unwrap();
        let report = liquidity.execute(&prepared).await.unwrap();

        assert_eq!(report.event, Some(LiquidityEvent::Closed));
    }

    #[tokio::test]
    async fn what_the_position_cannot_take_is_refused_before_sending() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(owner),
                liquidity: 1_000,
                owed: (0, 3),
                ..Chain::default()
            },
            json!([]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let above = LiquidityCommand::Remove {
            position: position(),
            liquidity: 1_001,
            min_out: TokenPair::default(),
        };
        let err = liquidity
            .prepare(&above, &request(owner))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("above"), "{err}");
        let close = LiquidityCommand::Close {
            position: position(),
        };
        let err = liquidity
            .prepare(&close, &request(owner))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holds liquidity"), "{err}");
        assert!(!sent_anything(&server).await);
    }

    #[tokio::test]
    async fn a_reverted_command_carries_its_reason_and_cost_and_no_event() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(owner),
                ..Chain::default()
            },
            json!([]),
            Some("Price slippage check"),
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let remove = LiquidityCommand::Remove {
            position: position(),
            liquidity: 1_000,
            min_out: TokenPair::new(1, 1),
        };
        let prepared = liquidity.prepare(&remove, &request(owner)).await.unwrap();
        let report = liquidity.execute(&prepared).await.unwrap();

        crate::testkit::contract::assert_liquidity_shape(
            &report,
            crate::testkit::contract::Sends::Transactions,
        );
        match &report.outcome {
            Outcome::Reverted { reason } => assert_eq!(reason, "Price slippage check"),
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert!(report.tx_ref.is_some());
        assert_eq!(report.cost.native().fee, 200_000 * 1_000_000_000);
    }

    #[tokio::test]
    async fn prepare_refuses_a_position_owned_by_someone_else() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Chain {
                owner_of: Some(Address::repeat_byte(0x99)),
                ..Chain::default()
            },
            json!([]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let close = LiquidityCommand::Close {
            position: position(),
        };
        let err = liquidity
            .prepare(&close, &request(owner))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is owned by"));
    }

    #[tokio::test]
    async fn prepare_refuses_another_owner_or_another_network() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let liquidity = EvmLiquidity::new(connect(&server).await, MANAGER, ManagerAbi::UniswapV3);

        let err = liquidity
            .prepare(&open(), &request(Address::repeat_byte(0x99)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sending address"));

        let owner = liquidity.sender().address();
        let LiquidityCommand::Open { range, deposit } = open() else {
            unreachable!()
        };
        let mainnet = LiquidityCommand::Open {
            range: Range {
                network: Network::evm(1),
                ..range
            },
            deposit,
        };
        let err = liquidity
            .prepare(&mainnet, &request(owner))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("eip155:1"), "{err}");
    }
}
