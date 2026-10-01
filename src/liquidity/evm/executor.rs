//! `EvmLiquidity` — the liquidity port over an `Arc<EvmSender>` (`SPEC.md`
//! §5b, `IMPLEMENTATION_PLAN.md` Phase 13).
//!
//! **One implementation, two senders.** A position's life is a sequence of
//! actions, each depending on the state the last one left, and an
//! `eth_call` discards that state — so unlike a swap there is no separate
//! `eth_call` simulator. Given a signing sender this is Live; given a fork
//! sender ([`EvmSender::fork`]) it runs the same transactions on an anvil
//! fork as an impersonated owner, and is Simulated. Every line of encoding
//! and decoding is shared, which is what the contract suite proves.
//!
//! **Reading the outcome.** Amounts come from the manager's own events
//! (`log.address == manager`): the pool emits `Mint` and `Collect` events of
//! its own with other signatures, and those are ignored. They are then
//! **cross-checked** against the ERC-20 `Transfer` logs of the same receipt —
//! for `Mint` and `Increase` the owner must have sent exactly the reported
//! amounts, for `Collect` it must have received exactly them. A mismatch
//! (a fee-on-transfer token, a wrong decoder) or a missing event is
//! [`LandedUnread`], never a guessed number.
//!
//! **Approvals** go to the manager, for exactly the desired amount of each
//! token, never `U256::MAX`. Any unspent part stays behind, capped at that
//! desired amount, and the next action approves exactly what it needs.

use crate::dex::{ChainAmount, Outcome, Prepared};
use crate::evm::erc20;
use crate::evm::rpc::{address_from_slice, BlockTag};
use crate::evm::{prepared_key, EvmSender, RpcLog, TxOutcome};
use crate::liquidity::evm::abi::{manager, slipstream, uniswap_v3};
use crate::liquidity::{
    unix_now, validate, LandedUnread, LiquidityAction, LiquidityExecutor, LiquidityRealised,
    LiquidityRequest, PoolKey, PositionRef,
};
use crate::Provenance;
use alloy_primitives::aliases::{I24, U160, U24};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mint,
    Increase,
    Decrease,
    Collect,
    Burn,
}

/// Everything `execute()` needs that isn't in `Prepared`, stashed by
/// `prepare()` and consumed exactly once.
#[derive(Debug, Clone)]
struct PendingAction {
    kind: Kind,
    chain_id: u64,
    manager: Address,
    owner: Address,
    deadline_unix_secs: u64,
    /// The pool's tokens, when this action moves them: `Mint`, `Increase`
    /// and `Collect`.
    tokens: Option<(Address, Address)>,
    /// What `Mint` and `Increase` approve, per token.
    desired: (u128, u128),
    /// The position's id; `None` for `Mint`, which learns it from the
    /// receipt.
    id: Option<U256>,
}

pub struct EvmLiquidity {
    sender: Arc<EvmSender>,
    pending: Mutex<HashMap<B256, PendingAction>>,
}

impl EvmLiquidity {
    pub fn new(sender: Arc<EvmSender>) -> Self {
        Self {
            sender,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn sender(&self) -> &Arc<EvmSender> {
        &self.sender
    }

    /// `ownerOf(id)` on the manager must be `owner`.
    async fn check_owner(&self, manager: Address, id: U256, owner: Address) -> Result<()> {
        let data = self
            .sender
            .rpc()
            .eth_call(
                manager,
                &manager::ownerOfCall { tokenId: id }.abi_encode(),
                None,
                BlockTag::Latest,
                None,
            )
            .await
            .with_context(|| format!("reading ownerOf({id}) on {manager}"))?;
        let holder = manager::ownerOfCall::abi_decode_returns(&data)
            .with_context(|| format!("decoding ownerOf({id}) on {manager}"))?;
        if holder != owner {
            bail!("position {id} on {manager} is owned by {holder}, not by {owner}");
        }
        Ok(())
    }

    /// `(token0, token1)` from `positions(id)`: words 2 and 3 of its return,
    /// the same on every supported manager.
    async fn position_tokens(&self, manager: Address, id: U256) -> Result<(Address, Address)> {
        let data = self
            .sender
            .rpc()
            .eth_call(
                manager,
                &manager::positionsCall { tokenId: id }.abi_encode(),
                None,
                BlockTag::Latest,
                None,
            )
            .await
            .with_context(|| format!("reading positions({id}) on {manager}"))?;
        let word = |i: usize| {
            data.get(i * 32..(i + 1) * 32)
                .ok_or_else(|| anyhow!("positions({id}) returned only {} bytes", data.len()))
        };
        Ok((
            Address::from_slice(&word(2)?[12..]),
            Address::from_slice(&word(3)?[12..]),
        ))
    }

    fn tx_ref(&self, tx_hash: B256) -> Option<Vec<u8>> {
        (self.sender.provenance() == Provenance::Landed).then(|| tx_hash.as_slice().to_vec())
    }

    fn unsettled(&self, outcome: Outcome, at: u64, tx_hash: B256) -> LiquidityRealised {
        LiquidityRealised {
            outcome,
            position: None,
            liquidity_delta: None,
            amount0: None,
            amount1: None,
            at,
            provenance: self.sender.provenance(),
            tx_ref: self.tx_ref(tx_hash),
        }
    }

    /// Reads a successful action's outcome from its receipt, or explains
    /// why it cannot (which the caller turns into `LandedUnread`).
    fn read_success(
        &self,
        ctx: &PendingAction,
        logs: &[RpcLog],
    ) -> std::result::Result<(U256, u128, u128, u128), String> {
        let (id, liquidity, amount0, amount1) = match ctx.kind {
            Kind::Mint => {
                let id = find_nft_transfer(logs, ctx.manager, Address::ZERO, ctx.owner)
                    .ok_or("no ERC-721 Transfer of a new position to the owner")?;
                let event = find_event::<manager::IncreaseLiquidity>(logs, ctx.manager, id)
                    .ok_or("no IncreaseLiquidity event for the new position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Increase => {
                let id = ctx.id.expect("Increase always has an id");
                let event = find_event::<manager::IncreaseLiquidity>(logs, ctx.manager, id)
                    .ok_or("no IncreaseLiquidity event for the position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Decrease => {
                let id = ctx.id.expect("Decrease always has an id");
                let event = find_event::<manager::DecreaseLiquidity>(logs, ctx.manager, id)
                    .ok_or("no DecreaseLiquidity event for the position")?;
                (id, event.liquidity, event.amount0, event.amount1)
            }
            Kind::Collect => {
                let id = ctx.id.expect("Collect always has an id");
                let event = find_event::<manager::Collect>(logs, ctx.manager, id)
                    .ok_or("no Collect event for the position")?;
                (id, 0, event.amount0, event.amount1)
            }
            Kind::Burn => {
                let id = ctx.id.expect("Burn always has an id");
                find_nft_transfer(logs, ctx.manager, ctx.owner, Address::ZERO)
                    .filter(|burnt| *burnt == id)
                    .ok_or("no ERC-721 Transfer of the position to the zero address")?;
                (id, 0, U256::ZERO, U256::ZERO)
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
        Ok((
            id,
            liquidity,
            narrow("amount0", amount0)?,
            narrow("amount1", amount1)?,
        ))
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

fn position_id(position: &PositionRef) -> Result<U256> {
    if position.id.len() != 32 {
        bail!(
            "PositionRef.id must be 32 big-endian bytes, got {}",
            position.id.len()
        );
    }
    Ok(U256::from_be_slice(&position.id))
}

fn int24(name: &str, value: i32) -> Result<I24> {
    I24::try_from(value).map_err(|_| anyhow!("{name} {value} does not fit in an int24"))
}

#[async_trait]
impl LiquidityExecutor for EvmLiquidity {
    async fn prepare(&self, action: &LiquidityAction, req: &LiquidityRequest) -> Result<Prepared> {
        validate(action, req)?;
        let owner = address_from_slice(&req.owner).context("LiquidityRequest.owner")?;
        if owner != self.sender.address() {
            bail!(
                "LiquidityRequest.owner ({owner}) is not this EvmLiquidity's sending address ({}) \
                 — it can only act as its own sender",
                self.sender.address()
            );
        }
        if action.chain_id() != self.sender.chain_id() {
            bail!(
                "the action is for chain {}, but this EvmLiquidity's sender is on chain {}",
                action.chain_id(),
                self.sender.chain_id()
            );
        }
        let manager = address_from_slice(action.manager()).context("manager")?;
        let deadline = U256::from(req.deadline_unix_secs);

        let (calldata, pending) = match action {
            LiquidityAction::Mint {
                range,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
            } => {
                let token0 = address_from_slice(&range.token0).context("RangeSpec.token0")?;
                let token1 = address_from_slice(&range.token1).context("RangeSpec.token1")?;
                let tick_lower = int24("tick_lower", range.tick_lower)?;
                let tick_upper = int24("tick_upper", range.tick_upper)?;
                let calldata = match range.pool_key {
                    PoolKey::Fee(fee) => uniswap_v3::mintCall {
                        params: uniswap_v3::MintParams {
                            token0,
                            token1,
                            fee: U24::try_from(fee)
                                .map_err(|_| anyhow!("fee {fee} does not fit in a uint24"))?,
                            tickLower: tick_lower,
                            tickUpper: tick_upper,
                            amount0Desired: U256::from(*amount0_desired),
                            amount1Desired: U256::from(*amount1_desired),
                            amount0Min: U256::from(*amount0_min),
                            amount1Min: U256::from(*amount1_min),
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
                            amount0Desired: U256::from(*amount0_desired),
                            amount1Desired: U256::from(*amount1_desired),
                            amount0Min: U256::from(*amount0_min),
                            amount1Min: U256::from(*amount1_min),
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
                    PendingAction {
                        kind: Kind::Mint,
                        chain_id: range.chain_id,
                        manager,
                        owner,
                        deadline_unix_secs: req.deadline_unix_secs,
                        tokens: Some((token0, token1)),
                        desired: (*amount0_desired, *amount1_desired),
                        id: None,
                    },
                )
            }
            LiquidityAction::Increase {
                position,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
            } => {
                let id = position_id(position)?;
                self.check_owner(manager, id, owner).await?;
                let tokens = self.position_tokens(manager, id).await?;
                let calldata = manager::increaseLiquidityCall {
                    params: manager::IncreaseLiquidityParams {
                        tokenId: id,
                        amount0Desired: U256::from(*amount0_desired),
                        amount1Desired: U256::from(*amount1_desired),
                        amount0Min: U256::from(*amount0_min),
                        amount1Min: U256::from(*amount1_min),
                        deadline,
                    },
                }
                .abi_encode();
                (
                    calldata,
                    PendingAction {
                        kind: Kind::Increase,
                        chain_id: position.chain_id,
                        manager,
                        owner,
                        deadline_unix_secs: req.deadline_unix_secs,
                        tokens: Some(tokens),
                        desired: (*amount0_desired, *amount1_desired),
                        id: Some(id),
                    },
                )
            }
            LiquidityAction::Decrease {
                position,
                liquidity,
                amount0_min,
                amount1_min,
            } => {
                let id = position_id(position)?;
                self.check_owner(manager, id, owner).await?;
                let calldata = manager::decreaseLiquidityCall {
                    params: manager::DecreaseLiquidityParams {
                        tokenId: id,
                        liquidity: *liquidity,
                        amount0Min: U256::from(*amount0_min),
                        amount1Min: U256::from(*amount1_min),
                        deadline,
                    },
                }
                .abi_encode();
                (
                    calldata,
                    PendingAction {
                        kind: Kind::Decrease,
                        chain_id: position.chain_id,
                        manager,
                        owner,
                        deadline_unix_secs: req.deadline_unix_secs,
                        tokens: None,
                        desired: (0, 0),
                        id: Some(id),
                    },
                )
            }
            LiquidityAction::Collect {
                position,
                amount0_max,
                amount1_max,
            } => {
                let id = position_id(position)?;
                self.check_owner(manager, id, owner).await?;
                let tokens = self.position_tokens(manager, id).await?;
                let calldata = manager::collectCall {
                    params: manager::CollectParams {
                        tokenId: id,
                        recipient: owner,
                        amount0Max: *amount0_max,
                        amount1Max: *amount1_max,
                    },
                }
                .abi_encode();
                (
                    calldata,
                    PendingAction {
                        kind: Kind::Collect,
                        chain_id: position.chain_id,
                        manager,
                        owner,
                        deadline_unix_secs: req.deadline_unix_secs,
                        tokens: Some(tokens),
                        desired: (0, 0),
                        id: Some(id),
                    },
                )
            }
            LiquidityAction::Burn { position } => {
                let id = position_id(position)?;
                self.check_owner(manager, id, owner).await?;
                (
                    manager::burnCall { tokenId: id }.abi_encode(),
                    PendingAction {
                        kind: Kind::Burn,
                        chain_id: position.chain_id,
                        manager,
                        owner,
                        deadline_unix_secs: req.deadline_unix_secs,
                        tokens: None,
                        desired: (0, 0),
                        id: Some(id),
                    },
                )
            }
        };

        let prepared = Prepared {
            to: manager.as_slice().to_vec(),
            calldata,
            value: 0,
        };
        self.pending
            .lock()
            .unwrap()
            .insert(prepared_key(&prepared), pending);
        Ok(prepared)
    }

    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityRealised> {
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&prepared_key(prepared))
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

        if matches!(ctx.kind, Kind::Mint | Kind::Increase) {
            let (token0, token1) = ctx.tokens.expect("Mint and Increase know their tokens");
            for (token, desired) in [(token0, ctx.desired.0), (token1, ctx.desired.1)] {
                if desired == 0 {
                    continue;
                }
                let amount = U256::from(desired);
                self.sender.ensure_balance(token, amount).await?;
                self.sender
                    .ensure_allowance(token, ctx.manager, amount)
                    .await?;
            }
        }

        match self
            .sender
            .send_and_confirm(ctx.manager, prepared.calldata.clone(), U256::ZERO)
            .await?
        {
            TxOutcome::Success {
                block,
                tx_hash,
                logs,
            } => {
                let (id, liquidity_delta, amount0, amount1) = self
                    .read_success(&ctx, &logs)
                    .map_err(|reason| LandedUnread {
                        tx_ref: tx_hash.as_slice().to_vec(),
                        reason,
                    })?;
                Ok(LiquidityRealised {
                    outcome: Outcome::Success,
                    position: Some(PositionRef {
                        chain_id: ctx.chain_id,
                        manager: ctx.manager.as_slice().to_vec(),
                        id: id.to_be_bytes::<32>().to_vec(),
                    }),
                    liquidity_delta: Some(liquidity_delta),
                    amount0: Some(amount0 as ChainAmount),
                    amount1: Some(amount1 as ChainAmount),
                    at: block,
                    provenance: self.sender.provenance(),
                    tx_ref: self.tx_ref(tx_hash),
                })
            }
            TxOutcome::Reverted {
                block,
                tx_hash,
                reason,
            } => Ok(self.unsettled(Outcome::Reverted { reason }, block, tx_hash)),
            TxOutcome::TimedOut { tx_hash } => {
                let at = self.sender.rpc().block_number().await.unwrap_or(0);
                Ok(self.unsettled(Outcome::TimedOut, at, tx_hash))
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
    use crate::liquidity::RangeSpec;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MANAGER: Address = Address::repeat_byte(0x11);
    const POOL: Address = Address::repeat_byte(0x22);
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

    /// A mock node for a signing sender whose owner holds and has approved
    /// everything, owns position `ID`, whose tokens are `TOKEN0`/`TOKEN1`,
    /// and whose one transaction lands with `logs` (or reverts with
    /// `revert`).
    async fn node(owner_of: Option<Address>, logs: Value, revert: Option<&str>) -> MockServer {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let owner_of = owner_of.map(|o| format_u256(U256::from_be_slice(&pad_address(o))));
        let reverts = revert.is_some();
        let revert = revert.map(|r| hex_data(&encode_error_string(r)));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let data = body["params"][0]["data"].as_str().unwrap_or("").to_string();
                let starts = |selector: [u8; 4]| data.starts_with(&hex_data(&selector));
                let result = if starts(BALANCE_OF_SELECTOR) || starts(ALLOWANCE_SELECTOR) {
                    format_u256(U256::MAX)
                } else if starts(manager::ownerOfCall::SELECTOR) {
                    owner_of.clone().unwrap_or_else(|| format_u256(U256::ZERO))
                } else if starts(manager::positionsCall::SELECTOR) {
                    let mut words = vec![0u8; 12 * 32];
                    words[2 * 32..3 * 32].copy_from_slice(&pad_address(TOKEN0));
                    words[3 * 32..4 * 32].copy_from_slice(&pad_address(TOKEN1));
                    hex_data(&words)
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
            json!({ "status": status, "blockNumber": "0x2a", "logs": logs }),
        )
        .await;
        server
    }

    fn request(owner: Address) -> LiquidityRequest {
        LiquidityRequest {
            owner: owner.as_slice().to_vec(),
            deadline_unix_secs: unix_now() + 600,
        }
    }

    fn mint() -> LiquidityAction {
        LiquidityAction::Mint {
            range: RangeSpec {
                chain_id: SEPOLIA,
                manager: MANAGER.as_slice().to_vec(),
                token0: TOKEN0.as_slice().to_vec(),
                token1: TOKEN1.as_slice().to_vec(),
                pool_key: PoolKey::Fee(3_000),
                tick_lower: -600,
                tick_upper: 600,
            },
            amount0_desired: 100,
            amount1_desired: 100,
            amount0_min: 0,
            amount1_min: 0,
        }
    }

    fn position() -> PositionRef {
        PositionRef {
            chain_id: SEPOLIA,
            manager: MANAGER.as_slice().to_vec(),
            id: U256::from(ID).to_be_bytes::<32>().to_vec(),
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
        )
    }

    #[tokio::test]
    async fn a_mint_reads_the_new_position_from_the_managers_events() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            None,
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
        let prepared = liquidity.prepare(&mint(), &request(owner)).await.unwrap();
        let realised = liquidity.execute(&prepared).await.unwrap();

        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.position, Some(position()));
        assert_eq!(realised.liquidity_delta, Some(1_000));
        assert_eq!((realised.amount0, realised.amount1), (Some(50), Some(60)));
        assert_eq!(realised.at, 42);
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        assert_eq!(liquidity.label(), "evm-liquidity-live");
    }

    #[tokio::test]
    async fn a_fee_on_transfer_token_is_landed_unread() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            None,
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
        let prepared = liquidity.prepare(&mint(), &request(owner)).await.unwrap();
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
        let server = node(None, json!([nft_transfer(Address::ZERO, owner, ID)]), None).await;
        let liquidity = liquidity_on(&server, signer).await;
        let prepared = liquidity.prepare(&mint(), &request(owner)).await.unwrap();
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
            None,
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
        let prepared = liquidity.prepare(&mint(), &request(owner)).await.unwrap();
        let err = liquidity.execute(&prepared).await.unwrap_err();

        let unread = err.downcast_ref::<LandedUnread>().expect("LandedUnread");
        assert!(unread.reason.contains("does not fit in u128"));
    }

    #[tokio::test]
    async fn a_collect_is_cross_checked_against_what_the_owner_received() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Some(owner),
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
        let action = LiquidityAction::Collect {
            position: position(),
            amount0_max: u128::MAX,
            amount1_max: u128::MAX,
        };
        let prepared = liquidity.prepare(&action, &request(owner)).await.unwrap();
        let realised = liquidity.execute(&prepared).await.unwrap();

        assert_eq!(realised.liquidity_delta, Some(0));
        assert_eq!((realised.amount0, realised.amount1), (Some(75), Some(90)));
    }

    #[tokio::test]
    async fn a_burn_moves_nothing_and_changes_no_liquidity() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(
            Some(owner),
            json!([nft_transfer(owner, Address::ZERO, ID)]),
            None,
        )
        .await;
        let liquidity = liquidity_on(&server, signer).await;
        let action = LiquidityAction::Burn {
            position: position(),
        };
        let prepared = liquidity.prepare(&action, &request(owner)).await.unwrap();
        let realised = liquidity.execute(&prepared).await.unwrap();

        assert_eq!(
            (realised.liquidity_delta, realised.amount0, realised.amount1),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(realised.position, Some(position()));
    }

    #[tokio::test]
    async fn a_reverted_action_carries_its_reason_and_no_amounts() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(Some(owner), json!([]), Some("Not cleared")).await;
        let liquidity = liquidity_on(&server, signer).await;
        let action = LiquidityAction::Burn {
            position: position(),
        };
        let prepared = liquidity.prepare(&action, &request(owner)).await.unwrap();
        let realised = liquidity.execute(&prepared).await.unwrap();

        crate::testkit::contract::assert_liquidity_shape(&realised);
        match realised.outcome {
            Outcome::Reverted { reason } => assert_eq!(reason, "Not cleared"),
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert!(realised.tx_ref.is_some());
    }

    #[tokio::test]
    async fn prepare_refuses_a_position_owned_by_someone_else() {
        let signer = unique_test_signer();
        let owner = signer.address();
        let server = node(Some(Address::repeat_byte(0x99)), json!([]), None).await;
        let liquidity = liquidity_on(&server, signer).await;
        let action = LiquidityAction::Burn {
            position: position(),
        };
        let err = liquidity
            .prepare(&action, &request(owner))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is owned by"));
    }

    #[tokio::test]
    async fn prepare_refuses_another_owner_or_another_chain() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let liquidity = EvmLiquidity::new(connect(&server).await);

        let err = liquidity
            .prepare(&mint(), &request(Address::repeat_byte(0x99)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sending address"));

        let owner = liquidity.sender().address();
        let LiquidityAction::Mint { range, .. } = mint() else {
            unreachable!()
        };
        let mainnet = LiquidityAction::Mint {
            range: RangeSpec {
                chain_id: 1,
                ..range
            },
            amount0_desired: 1,
            amount1_desired: 1,
            amount0_min: 0,
            amount1_min: 0,
        };
        let err = liquidity
            .prepare(&mainnet, &request(owner))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("chain 1"));
    }
}
