//! `EvmSimulated` — runs a prepared call against live chain state via
//! `eth_call`'s state-override parameter, with no transaction ever
//! broadcast (`SPEC.md` §5, second required implementation).
//!
//! This needs no forked node: `eth_call`'s optional third parameter lets a
//! caller override specific storage slots for the duration of one call,
//! against whatever block the node already has (default: latest). Every
//! major EVM client (Geth, Erigon, Reth) and hosted RPC provider supports
//! it, including free/keyless public endpoints, so this adapter needs
//! nothing beyond an RPC URL.
//!
//! **Which block.** `execute(prepared, Some(n))` runs at block `n`. With
//! `None` it runs at the latest block, read once and then used for every
//! call of that dry run so the probes and the swap see one state; or, after
//! [`EvmSimulated::with_pending_block`], at the node's `pending` block (on
//! Base, the preconfirmed state), for a caller that priced from it.
//! `Realised::at` is that block's number: for `pending`, the number the node
//! gives its pending block.
//!
//! **The amount in.** An `eth_call` shows no transfers, so what the swap
//! consumed is what the router says it did, or else what it was offered: the
//! dry run gives the payer exactly `route.amount_in`, and `Realised::amount_in`
//! is that offer unless an [`InputRule`] is set for the address called. A
//! contract that takes less than it was given when the pool's price limit is
//! reached (`PoolSwapper.swapV3`) returns what it took, and its caller sets
//! [`InputRule::Word`] for it with [`EvmSimulated::with_input_rule`]. A swap
//! that reverted took none.
//!
//! **Rules per function.** One address can hold functions that return
//! different layouts (`PoolSwapper.swapV3` returns `(amountInUsed, amountOut)`,
//! its `swapV2` just `amountOut`), so a rule can also be set for an address
//! *and* a 4-byte selector, the first 4 bytes of the call's calldata
//! ([`EvmSimulated::with_return_rule_for`], [`EvmSimulated::with_input_rule_for`]).
//! A call is read by the rule for its (address, selector), else by the
//! address's own rule (the one set without a selector), else by the default.
//!
//! **Gas.** A swap that ran is also put to the node's `eth_estimateGas`, with
//! the same sender, block and overrides, and `EvmCost::gas_used` is its
//! answer: what the swap needs, which is at least what it uses. A swap that
//! reverted reports no gas.
//!
//! Where the simulated sender does not actually hold the input token or
//! the router's allowance, this adapter overrides just enough storage to
//! make the call possible: the sender's `balanceOf` slot, and the
//! `allowance` slot for the sender *and the router the route actually
//! calls* (never the address probing it — SPEC.md §5's explicit pitfall).
//! Neither ERC-20 storage layout is standardized, so both slots are probed
//! for per token the first time they're needed, then cached
//! ([`crate::evm::erc20`]).
//!
//! **A contract payer** (`Payer::CalledContract`): the contract the route
//! calls holds the input and pays the venue itself, so the balance goes on
//! that contract and no allowance is written. A contract not yet deployed
//! is placed by a [`CodeOverride`] at its address: its runtime code and the
//! storage its constructor would have written, in every call this adapter
//! makes.
//!
//! **Adapter convention for `RouteQuote.payload`:** SPEC.md leaves the
//! payload's shape entirely up to the adapter that consumes it (§5). This
//! adapter expects it to be `router_address (20 bytes) ++ calldata` — the
//! simplest encoding that carries what a generic (not router-specific)
//! implementation needs. A concrete router integration is free to use a
//! different convention as long as its own adapter agrees with whatever
//! produces its `RouteQuote`s.
//!
//! **Return-value convention:** `amount_out` is the **first 32-byte word**
//! of the router's return data unless a [`ReturnRule`] is set for the
//! address called. `SwapRouter02.exactInputSingle` returns one word;
//! KyberSwap's `MetaAggregationRouterV2.swap` returns
//! `(uint256 returnAmount, uint256 gasUsed)`, and the other common
//! aggregator routers likewise put the output amount first. A router that
//! returns every hop's amount as one `uint256[]` (Uniswap v2's and
//! Aerodrome's `swapExactTokensForTokens`) has its first word read as the
//! array's offset, so its caller sets [`ReturnRule::LastOfArray`] for it
//! with [`EvmSimulated::with_return_rule`]. A function that puts it
//! elsewhere is read by [`ReturnRule::Word`]. Return data the rule cannot
//! decode is an error, never a panic.

use crate::dex::{
    ChainAmount, DexExecutor, EvmCall, EvmCost, Outcome, Payer, Prepared, Realised, RouteQuote,
    SwapRequest, TxCost,
};
use crate::evm::erc20::{self, SlotCache};
use crate::evm::prepared_key;
use crate::evm::rpc::{
    address_from_slice, first_word, format_u256, hex_data, last_of_uint_array, BlockTag, EvmRpc,
    RpcError,
};
use crate::{Network, Provenance};
use alloy_primitives::{Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// Everything `execute()` needs that isn't in `Prepared`, stashed by
/// `prepare()` and consumed exactly once.
struct PendingSimulation {
    token_in: Address,
    sender: Address,
    payer: Payer,
    amount_in: ChainAmount,
}

/// A contract as it would be once deployed, for a simulation of a chain
/// that does not have it yet: its runtime code, and the storage its
/// constructor would have written (an owner, an operator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeOverride {
    pub runtime_code: Vec<u8>,
    /// Slot and value, each a 32-byte word.
    pub storage: Vec<(B256, B256)>,
}

/// Where the amount out is in a call's return data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReturnRule {
    /// The first 32-byte word: one `uint256`, or a tuple that puts the
    /// amount out first.
    #[default]
    FirstWord,
    /// The last element of a returned `uint256[]`: a router that returns
    /// every hop's amount, the amount out last.
    LastOfArray,
    /// The `n`th (from 0) 32-byte word of the return data, for a function
    /// that returns the amount out after something else, as
    /// `PoolSwapper.swapV3` returns `(amountInUsed, amountOut)`. A word that
    /// is missing, or does not fit a `u128`, is an error, never a figure.
    Word(usize),
}

impl ReturnRule {
    fn amount_out(self, data: &[u8]) -> Result<U256> {
        match self {
            ReturnRule::FirstWord => first_word(data),
            ReturnRule::LastOfArray => last_of_uint_array(data),
            ReturnRule::Word(n) => nth_word(data, n, "an amount out"),
        }
    }
}

/// The `n`th (from 0) 32-byte word of `data`; `names` is what the caller
/// takes it for, for the error when `data` has no such word.
fn nth_word(data: &[u8], n: usize, names: &str) -> Result<U256> {
    n.checked_mul(32)
        .and_then(|from| data.get(from..from.checked_add(32)?))
        .map(U256::from_be_slice)
        .ok_or_else(|| {
            anyhow!(
                "no word {n} in {} bytes of return data: it names no {names}",
                data.len()
            )
        })
}

/// Where the input a swap consumed is, for a router that returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputRule {
    /// The router returns no such figure: the swap is taken to have consumed
    /// its whole offer, `route.amount_in`, which is what the dry run gave the
    /// payer. Right for a router that spends the exact input it is given
    /// (`SwapRouter02.exactInputSingle`), and a guess for one that can take
    /// less without saying so.
    #[default]
    Offered,
    /// The `n`th (from 0) 32-byte word of the return data is the input the
    /// router took, as `PoolSwapper.swapV3` returns `amountInUsed`. A word
    /// that is missing, does not fit a `u128`, or is more than was offered is
    /// an error, never a figure.
    Word(usize),
}

impl InputRule {
    fn amount_in(self, data: &[u8], offered: ChainAmount) -> Result<ChainAmount> {
        match self {
            InputRule::Offered => Ok(offered),
            InputRule::Word(n) => {
                let taken: ChainAmount = nth_word(data, n, "input taken")?
                    .try_into()
                    .context("the input taken overflowed u128")?;
                if taken > offered {
                    bail!("the router says it took {taken} of an offer of {offered}");
                }
                Ok(taken)
            }
        }
    }
}

/// The rules of one kind, set for an address (whatever it is called with) or
/// for an address and the 4-byte selector of the function called.
#[derive(Debug)]
struct Rules<R> {
    by_call: HashMap<(Address, Option<[u8; 4]>), R>,
}

impl<R> Default for Rules<R> {
    fn default() -> Self {
        Self {
            by_call: HashMap::new(),
        }
    }
}

impl<R: Copy + Default> Rules<R> {
    fn set(&mut self, address: Address, selector: Option<[u8; 4]>, rule: R) {
        self.by_call.insert((address, selector), rule);
    }

    /// The rule for a call of `address` with `calldata`: the one set for its
    /// selector, else the one set for the address, else the default. Calldata
    /// shorter than a selector has none, so only the last two apply.
    fn for_call(&self, address: Address, calldata: &[u8]) -> R {
        let selector = calldata.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok());
        selector
            .and_then(|selector| self.by_call.get(&(address, Some(selector))))
            .or_else(|| self.by_call.get(&(address, None)))
            .copied()
            .unwrap_or_default()
    }
}

pub struct EvmSimulated {
    rpc: EvmRpc,
    pending: Mutex<HashMap<B256, PendingSimulation>>,
    slots: SlotCache,
    code: HashMap<Address, CodeOverride>,
    returns: Rules<ReturnRule>,
    inputs: Rules<InputRule>,
    /// Whether a run with no block given reads the node's `pending` block
    /// rather than its latest one.
    pending_block: bool,
}

impl EvmSimulated {
    pub fn new(rpc: EvmRpc) -> Self {
        Self {
            rpc,
            pending: Mutex::new(HashMap::new()),
            slots: SlotCache::default(),
            code: HashMap::new(),
            returns: Rules::default(),
            inputs: Rules::default(),
            pending_block: false,
        }
    }

    /// Runs at the node's `pending` block when `execute` is given no block,
    /// instead of at the latest one: for a caller whose decision was priced
    /// from the pending state (Base's Flashblocks), which a dry run on the
    /// latest block would disagree with. A block given to `execute` still
    /// pins the run to it.
    pub fn with_pending_block(mut self) -> Self {
        self.pending_block = true;
        self
    }

    /// Reads the amount out of a call to `address` by `rule` rather than
    /// from its first word: for every function of `address` that has no rule
    /// of its own ([`with_return_rule_for`](Self::with_return_rule_for)).
    pub fn with_return_rule(mut self, address: Address, rule: ReturnRule) -> Self {
        self.returns.set(address, None, rule);
        self
    }

    /// As [`with_return_rule`](Self::with_return_rule), for calls to `address`
    /// whose calldata starts with `selector` only, and in preference to the
    /// rule set for the address as a whole.
    pub fn with_return_rule_for(
        mut self,
        address: Address,
        selector: [u8; 4],
        rule: ReturnRule,
    ) -> Self {
        self.returns.set(address, Some(selector), rule);
        self
    }

    /// Reads the input a swap consumed from a call to `address` by `rule`
    /// rather than taking the whole offer: for every function of `address`
    /// that has no rule of its own
    /// ([`with_input_rule_for`](Self::with_input_rule_for)).
    pub fn with_input_rule(mut self, address: Address, rule: InputRule) -> Self {
        self.inputs.set(address, None, rule);
        self
    }

    /// As [`with_input_rule`](Self::with_input_rule), for calls to `address`
    /// whose calldata starts with `selector` only, and in preference to the
    /// rule set for the address as a whole.
    pub fn with_input_rule_for(
        mut self,
        address: Address,
        selector: [u8; 4],
        rule: InputRule,
    ) -> Self {
        self.inputs.set(address, Some(selector), rule);
        self
    }

    /// Runs every call with `code` placed at `address`, whatever the chain
    /// holds there: a contract not yet deployed, simulated on the chain's
    /// real state.
    pub fn with_code_override(mut self, address: Address, code: CodeOverride) -> Self {
        self.code.insert(address, code);
        self
    }

    /// The state overrides for one swap: every code override, then the
    /// input's balance on whoever pays it, and an allowance to `router` only
    /// when that is the sender.
    async fn overrides(
        &self,
        ctx: &PendingSimulation,
        router: Address,
        tag: BlockTag,
    ) -> Result<Value> {
        let mut overrides = Map::new();
        for (address, code) in &self.code {
            let storage: Map<String, Value> = code
                .storage
                .iter()
                .map(|(slot, value)| (slot.to_string(), json!(value.to_string())))
                .collect();
            overrides.insert(
                address.to_string(),
                json!({ "code": hex_data(&code.runtime_code), "stateDiff": storage }),
            );
        }

        let holder = match ctx.payer {
            Payer::Sender => ctx.sender,
            Payer::CalledContract => router,
        };
        let amount_in = format_u256(U256::from(ctx.amount_in));
        let balance_slot =
            erc20::find_balance_slot(&self.rpc, &self.slots, ctx.token_in, holder, tag).await?;
        set_slot(&mut overrides, ctx.token_in, balance_slot, &amount_in);

        match ctx.payer {
            Payer::Sender => {
                let allowance_slot = erc20::find_allowance_slot(
                    &self.rpc,
                    &self.slots,
                    ctx.token_in,
                    ctx.sender,
                    router,
                    tag,
                )
                .await?;
                set_slot(&mut overrides, ctx.token_in, allowance_slot, &amount_in);
            }
            // The contract pays the venue from its own balance: no allowance.
            Payer::CalledContract => {}
        }
        Ok(Value::Object(overrides))
    }
}

/// Writes `value` into `slot` of `address`'s storage, in an override that
/// may already hold code or other slots for it.
fn set_slot(overrides: &mut Map<String, Value>, address: Address, slot: B256, value: &str) {
    let entry = overrides
        .entry(address.to_string())
        .or_insert_with(|| json!({}));
    let diff = entry
        .as_object_mut()
        .expect("every override is an object")
        .entry("stateDiff")
        .or_insert_with(|| json!({}));
    diff.as_object_mut()
        .expect("a stateDiff is an object")
        .insert(slot.to_string(), json!(value));
}

#[async_trait]
impl DexExecutor for EvmSimulated {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        if route.payload.len() < 20 {
            bail!(
                "route.payload must be at least 20 bytes (router address + calldata), got {}",
                route.payload.len()
            );
        }
        if let Network::Solana { .. } = route.network {
            bail!(
                "route is for network {}, but EvmSimulated runs EVM calls only",
                route.network
            );
        }
        req.priority.policy_only("EvmSimulated")?;
        let (router_bytes, calldata) = route.payload.split_at(20);
        let token_in = address_from_slice(&route.token_in).context("route.token_in")?;
        let sender = address_from_slice(&req.sender).context("req.sender")?;

        let call = EvmCall {
            to: router_bytes.to_vec(),
            calldata: calldata.to_vec(),
            value: 0,
        };

        self.pending.lock().unwrap().insert(
            prepared_key(&call),
            PendingSimulation {
                token_in,
                sender,
                payer: req.payer,
                amount_in: route.amount_in,
            },
        );
        Ok(Prepared::Evm(call))
    }

    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised> {
        let prepared = prepared.evm_call()?;
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&prepared_key(prepared))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this EvmSimulated instance did not \
                     produce, or already consumed"
                )
            })?;
        let router = address_from_slice(&prepared.to).context("prepared.to")?;

        let tag = match at {
            Some(block) => BlockTag::Number(block),
            None if self.pending_block => BlockTag::Pending,
            None => BlockTag::Number(self.rpc.block_number().await?),
        };
        let block = self.rpc.block_number_at(tag).await?;
        let overrides = self.overrides(&ctx, router, tag).await?;

        match self
            .rpc
            .eth_call(
                router,
                &prepared.calldata,
                Some(ctx.sender),
                tag,
                Some(&overrides),
            )
            .await
        {
            Ok(data) => {
                let rule = self.returns.for_call(router, &prepared.calldata);
                let amount_out: u128 = rule
                    .amount_out(&data)
                    .and_then(|out| u128::try_from(out).context("amount_out overflowed u128"))
                    .with_context(|| format!("decoding {router}'s return data by {rule:?}"))?;
                let input_rule = self.inputs.for_call(router, &prepared.calldata);
                let amount_in = input_rule
                    .amount_in(&data, ctx.amount_in)
                    .with_context(|| {
                        format!("reading the input {router} took by {input_rule:?}")
                    })?;
                // An `eth_call` reports no gas: ask the node what the same
                // call needs. A refusal here, of a call that just ran, is the
                // node disagreeing with itself (the pending state moved
                // between the two): neither answer is the swap's.
                let gas_used = self
                    .rpc
                    .eth_estimate_gas(
                        router,
                        &prepared.calldata,
                        Some(ctx.sender),
                        tag,
                        Some(&overrides),
                    )
                    .await
                    .map_err(|err| match err.downcast::<RpcError>() {
                        Ok(refused) => anyhow!(
                            "eth_estimateGas refused a swap that eth_call ran: {}",
                            refused.reason
                        ),
                        Err(transport) => transport.context("eth_estimateGas for the swap"),
                    })?;
                Ok(Realised {
                    amount_out: Some(amount_out),
                    amount_in: Some(amount_in),
                    outcome: Outcome::Success,
                    cost: TxCost::Evm(EvmCost {
                        gas_used: Some(gas_used),
                        ..EvmCost::default()
                    }),
                    at: block,
                    provenance: Provenance::Simulated,
                    tx_ref: None,
                })
            }
            Err(err) => match err.downcast::<RpcError>() {
                Ok(revert) => Ok(Realised {
                    amount_out: None,
                    amount_in: None,
                    outcome: Outcome::Reverted {
                        reason: revert.reason,
                    },
                    cost: TxCost::Evm(EvmCost::default()),
                    at: block,
                    provenance: Provenance::Simulated,
                    tx_ref: None,
                }),
                Err(transport_err) => Err(transport_err),
            },
        }
    }

    fn label(&self) -> &'static str {
        "evm-simulated"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::PriorityBid;
    use crate::evm::erc20::tests::mount_slot_probe;
    use crate::evm::erc20::{
        allowance_slot, mapping_slot, ALLOWANCE_SELECTOR, BALANCE_OF_SELECTOR,
    };
    use crate::evm::rpc::{decode_hex, encode_error_string, pad_address};
    use serde_json::Value as Json;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn route(payload: Vec<u8>) -> RouteQuote {
        RouteQuote {
            network: Network::evm(1),
            token_in: vec![0xAA; 20],
            token_out: vec![0xBB; 20],
            amount_in: 1_000,
            expected_amount_out: 900,
            payload,
        }
    }

    fn request() -> SwapRequest {
        SwapRequest {
            sender: vec![0xCC; 20],
            recipient: vec![0xDD; 20],
            payer: Payer::Sender,
            min_amount_out: 900,
            deadline_unix_secs: 0,
            priority: PriorityBid::Policy,
        }
    }

    fn payload_for(router: [u8; 20], calldata: &[u8]) -> Vec<u8> {
        let mut payload = router.to_vec();
        payload.extend_from_slice(calldata);
        payload
    }

    /// Mounts the balance and allowance probes for the fixed token, sender
    /// and `router`, and a router whose swap call (calldata exactly
    /// `calldata`) returns `swap_return` — any other `eth_call` gets zero.
    async fn mount_router(
        server: &MockServer,
        router: [u8; 20],
        calldata: &[u8],
        swap_return: Vec<u8>,
    ) {
        mount_probes(server, router).await;
        mount_swap(server, calldata, swap_return).await;
    }

    /// The balance and allowance probes for the fixed token and sender, and
    /// `router` as the spender.
    async fn mount_probes(server: &MockServer, router: [u8; 20]) {
        let sender = Address::from([0xCC; 20]);
        mount_slot_probe(server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;
        mount_slot_probe(
            server,
            ALLOWANCE_SELECTOR,
            allowance_slot(sender, Address::from(router), 0),
        )
        .await;
    }

    /// What the mock node's `eth_estimateGas` answers.
    const ESTIMATED_GAS: u64 = 123_456;

    /// A swap call (calldata exactly `calldata`) that returns `swap_return`,
    /// and an `eth_estimateGas` that answers [`ESTIMATED_GAS`]; any other
    /// `eth_call` gets zero.
    async fn mount_swap(server: &MockServer, calldata: &[u8], swap_return: Vec<u8>) {
        mount_swap_call(server, calldata, swap_return).await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_estimateGas" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "jsonrpc": "2.0", "id": 1, "result": format!("0x{ESTIMATED_GAS:x}") }),
            ))
            .mount(server)
            .await;
    }

    /// As [`mount_swap`], with no `eth_estimateGas` mounted.
    async fn mount_swap_call(server: &MockServer, calldata: &[u8], swap_return: Vec<u8>) {
        let expected_calldata = calldata.to_vec();
        let swap_return_hex = format!("0x{}", hex::encode(swap_return));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let data = decode_hex(body["params"][0]["data"].as_str().unwrap()).unwrap();
                let result = if data == expected_calldata {
                    swap_return_hex.clone()
                } else {
                    format_u256(U256::ZERO)
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(server)
            .await;
    }

    /// The probes for `router`, and a node whose swap calls answer by their
    /// calldata: each `(calldata, return data)` of `calls` returns its own
    /// data, any other `eth_call` gets zero. For one address that holds
    /// several functions.
    async fn mount_router_calls(
        server: &MockServer,
        router: [u8; 20],
        calls: Vec<(Vec<u8>, Vec<u8>)>,
    ) {
        mount_probes(server, router).await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let data = decode_hex(body["params"][0]["data"].as_str().unwrap()).unwrap();
                let result = calls
                    .iter()
                    .find(|(calldata, _)| *calldata == data)
                    .map_or_else(
                        || format_u256(U256::ZERO),
                        |(_, returned)| format!("0x{}", hex::encode(returned)),
                    );
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_estimateGas" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "jsonrpc": "2.0", "id": 1, "result": format!("0x{ESTIMATED_GAS:x}") }),
            ))
            .mount(server)
            .await;
    }

    /// `keccak256(signature)[..4]`: the selector of a function.
    fn selector_of(signature: &str) -> [u8; 4] {
        alloy_primitives::keccak256(signature.as_bytes())[..4]
            .try_into()
            .unwrap()
    }

    /// A call of the function `selector`, with one word of arguments.
    fn call_of(selector: [u8; 4], arg: u8) -> Vec<u8> {
        let mut calldata = selector.to_vec();
        calldata.extend_from_slice(&[arg; 32]);
        calldata
    }

    /// Return data of one 32-byte word per figure.
    fn words(figures: &[u64]) -> Vec<u8> {
        figures
            .iter()
            .flat_map(|figure| U256::from(*figure).to_be_bytes::<32>())
            .collect()
    }

    /// Prepares `calldata` to `router` for the fixed route and sender, and
    /// runs it at block 1.
    async fn run(adapter: &EvmSimulated, router: [u8; 20], calldata: &[u8]) -> Result<Realised> {
        let prepared = adapter
            .prepare(&route(payload_for(router, calldata)), &request())
            .await
            .unwrap();
        adapter.execute(&prepared, Some(1)).await
    }

    /// A simulation runs the call at the sender's policy: a bid above it is
    /// refused before anything is asked of the node.
    #[tokio::test]
    async fn prepare_refuses_a_bid_above_its_fee_policy() {
        let adapter = EvmSimulated::new(EvmRpc::new("http://127.0.0.1:9".to_string()));
        let bidding = SwapRequest {
            priority: PriorityBid::AbovePolicyPerGas(1),
            ..request()
        };
        let err = adapter
            .prepare(&route(payload_for([0x11; 20], &[1, 2, 3, 4])), &bidding)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("EvmSimulated sends at its fee policy only"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn execute_overrides_both_slots_and_decodes_amount_out() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let token = Address::from([0xAA; 20]);
        let router_addr = Address::from(router);
        mount_router(
            &server,
            router,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let route = route(payload_for(router, &calldata));
        let request = request();

        let prepared = adapter.prepare(&route, &request).await.unwrap();
        let call = prepared.evm_call().unwrap();
        assert_eq!(call.to, router.to_vec());
        assert_eq!(call.calldata, calldata);

        let realised = adapter.execute(&prepared, Some(42)).await.unwrap();
        assert_eq!(realised.amount_out, Some(777));
        assert_eq!(
            realised.amount_in,
            Some(1_000),
            "an eth_call shows no transfers: the offer, which the payer was given"
        );
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
        assert_eq!(realised.at, 42);

        // The allowance override must have gone to the router the route
        // actually calls (SPEC.md §5's explicit pitfall), not some other
        // address.
        let calls = eth_calls(&server).await;
        let overrides = &swap_call(&calls, &calldata)["params"][2][token.to_string()]["stateDiff"];
        let sender = Address::from([0xCC; 20]);
        assert_eq!(
            overrides[allowance_slot(sender, router_addr, 0).to_string()],
            format_u256(U256::from(1_000u64)),
            "the sender's allowance to the router"
        );
        assert_eq!(adapter.slots.allowance_base(token), Some(0u64.into()));
    }

    /// Every request of `method` a mock node received, as its JSON body.
    async fn requests(server: &MockServer, method: &str) -> Vec<Json> {
        server
            .received_requests()
            .await
            .expect("the mock records its requests")
            .iter()
            .map(|r| r.body_json::<Json>().unwrap())
            .filter(|body| body["method"] == method)
            .collect()
    }

    /// Every `eth_call` a mock node received, as its JSON body.
    async fn eth_calls(server: &MockServer) -> Vec<Json> {
        requests(server, "eth_call").await
    }

    /// The swap's own call: the one whose data is `calldata`.
    fn swap_call<'a>(calls: &'a [Json], calldata: &[u8]) -> &'a Json {
        calls
            .iter()
            .find(|body| {
                decode_hex(body["params"][0]["data"].as_str().unwrap()).unwrap() == calldata
            })
            .expect("the swap was called")
    }

    fn reads_an_allowance(calls: &[Json]) -> bool {
        calls.iter().any(|body| {
            body["params"][0]["data"]
                .as_str()
                .unwrap()
                .starts_with(&hex_data(&ALLOWANCE_SELECTOR))
        })
    }

    /// MORPHO on Base keeps its state under OpenZeppelin v5's ERC-7201
    /// namespace, not in a low slot: the balance override goes where the
    /// namespace puts the sender's balance, and the allowance override one
    /// slot on from it, to the router the route calls.
    #[tokio::test]
    async fn a_token_under_openzeppelins_namespace_is_overridden_there() {
        use crate::evm::erc20::{OZ_ERC20_ALLOWANCES, OZ_ERC20_NAMESPACE};

        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let token = Address::from([0xAA; 20]);
        let sender = Address::from([0xCC; 20]);
        let balance_slot = mapping_slot(sender, OZ_ERC20_NAMESPACE);
        let allowance = allowance_slot(sender, Address::from(router), OZ_ERC20_ALLOWANCES);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, balance_slot).await;
        mount_slot_probe(&server, ALLOWANCE_SELECTOR, allowance).await;
        mount_swap(
            &server,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = adapter.execute(&prepared, Some(42)).await.unwrap();
        assert_eq!(realised.amount_out, Some(777));

        let calls = eth_calls(&server).await;
        let diff = swap_call(&calls, &calldata)["params"][2][token.to_string()]["stateDiff"]
            .as_object()
            .unwrap();
        assert_eq!(diff.len(), 2, "the balance and the allowance: {diff:?}");
        let amount = format_u256(U256::from(1_000u64));
        assert_eq!(diff[&balance_slot.to_string()], amount);
        assert_eq!(diff[&allowance.to_string()], amount);
    }

    /// A contract that keeps its own inventory: the balance goes on the
    /// contract called, and no allowance is read or written.
    #[tokio::test]
    async fn a_contract_payer_holds_the_input_and_grants_no_allowance() {
        let server = MockServer::start().await;
        let contract = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let token = Address::from([0xAA; 20]);
        let holder_slot = mapping_slot(Address::from(contract), 0);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, holder_slot).await;
        mount_swap(
            &server,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let request = SwapRequest {
            payer: Payer::CalledContract,
            ..request()
        };
        let prepared = adapter
            .prepare(&route(payload_for(contract, &calldata)), &request)
            .await
            .unwrap();
        let realised = adapter.execute(&prepared, Some(42)).await.unwrap();
        assert_eq!(realised.amount_out, Some(777));

        let calls = eth_calls(&server).await;
        assert!(
            !reads_an_allowance(&calls),
            "a contract payer approves nothing"
        );
        let swap = swap_call(&calls, &calldata);
        let from: Address = swap["params"][0]["from"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            from,
            Address::from([0xCC; 20]),
            "the sender still makes the call"
        );
        let diff = swap["params"][2][token.to_string()]["stateDiff"]
            .as_object()
            .unwrap();
        assert_eq!(
            diff.len(),
            1,
            "the contract's balance and nothing else: {diff:?}"
        );
        assert_eq!(
            diff[&holder_slot.to_string()],
            format_u256(U256::from(1_000u64))
        );
    }

    /// A contract not yet deployed: its code and its constructor's storage
    /// are placed at its address, beside the input's balance on the token.
    #[tokio::test]
    async fn a_code_override_places_the_contract_and_its_storage_in_the_call() {
        let server = MockServer::start().await;
        let contract = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let token = Address::from([0xAA; 20]);
        let holder_slot = mapping_slot(Address::from(contract), 0);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, holder_slot).await;
        mount_swap(
            &server,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let operator = Address::from([0xCC; 20]).into_word();
        let (owner_slot, operator_slot) = (B256::ZERO, B256::with_last_byte(1));
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri())).with_code_override(
            Address::from(contract),
            CodeOverride {
                runtime_code: vec![0x60, 0x80, 0x60, 0x40, 0x52],
                storage: vec![(owner_slot, operator), (operator_slot, operator)],
            },
        );
        let request = SwapRequest {
            payer: Payer::CalledContract,
            ..request()
        };
        let prepared = adapter
            .prepare(&route(payload_for(contract, &calldata)), &request)
            .await
            .unwrap();
        assert_eq!(
            adapter
                .execute(&prepared, Some(42))
                .await
                .unwrap()
                .amount_out,
            Some(777)
        );

        let calls = eth_calls(&server).await;
        let overrides = &swap_call(&calls, &calldata)["params"][2];
        let placed = &overrides[Address::from(contract).to_string()];
        assert_eq!(placed["code"], "0x6080604052");
        assert_eq!(
            placed["stateDiff"][owner_slot.to_string()],
            operator.to_string()
        );
        assert_eq!(
            placed["stateDiff"][operator_slot.to_string()],
            operator.to_string()
        );
        assert_eq!(
            overrides[token.to_string()]["stateDiff"][holder_slot.to_string()],
            format_u256(U256::from(1_000u64))
        );
    }

    /// README defect 1: a router returning `(returnAmount, gasUsed)` — 64
    /// bytes with a non-zero first word — used to panic the adapter.
    #[tokio::test]
    async fn a_two_word_return_yields_its_first_word() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x0a, 0x0b];
        let mut swap_return = U256::from(123_456u64).to_be_bytes::<32>().to_vec();
        swap_return.extend_from_slice(&U256::from(98_765u64).to_be_bytes::<32>());
        mount_router(&server, router, &calldata, swap_return).await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = adapter.execute(&prepared, Some(1)).await.unwrap();

        assert_eq!(realised.amount_out, Some(123_456));
    }

    /// Aerodrome's router returns `[amountIn, amountOut]`: read by its first
    /// word, the answer is the array's offset (32), which is not an amount.
    #[tokio::test]
    async fn a_router_returning_every_hops_amount_is_read_by_its_return_rule() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x0d, 0x0e];
        let amounts = crate::evm::rpc::tests::uint_array(&[1_000, 1_234]);
        mount_router(&server, router, &calldata, amounts).await;

        let by_first_word = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = by_first_word
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = by_first_word.execute(&prepared, Some(1)).await.unwrap();
        assert_eq!(
            realised.amount_out,
            Some(32),
            "the offset word, not an amount"
        );

        let by_rule = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule(Address::from(router), ReturnRule::LastOfArray);
        let prepared = by_rule
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = by_rule.execute(&prepared, Some(1)).await.unwrap();
        assert_eq!(realised.amount_out, Some(1_234));
        assert!(matches!(realised.outcome, Outcome::Success));
    }

    /// A rule belongs to the address it was set for: another router called
    /// through the same adapter is still read by its first word.
    #[tokio::test]
    async fn a_return_rule_applies_only_to_its_own_address() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x0f];
        mount_router(
            &server,
            router,
            &calldata,
            U256::from(4_321u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule(Address::from([0x22; 20]), ReturnRule::LastOfArray);
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = adapter.execute(&prepared, Some(1)).await.unwrap();
        assert_eq!(realised.amount_out, Some(4_321));
    }

    /// `PoolSwapper.swapV3` returns what the pool took as well as what it
    /// paid, which is less than the offer when the price limit is reached: a
    /// rule for its address reads it, and another router is not affected.
    #[tokio::test]
    async fn an_input_rule_reads_what_the_router_says_it_took() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x21, 0x22];
        // (amount out, amount in): 600 of the 1 000 offered.
        let mut swap_return = U256::from(550u64).to_be_bytes::<32>().to_vec();
        swap_return.extend_from_slice(&U256::from(600u64).to_be_bytes::<32>());
        mount_router(&server, router, &calldata, swap_return).await;

        let by_rule = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_input_rule(Address::from(router), InputRule::Word(1));
        let prepared = by_rule
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = by_rule.execute(&prepared, Some(1)).await.unwrap();
        assert_eq!(realised.amount_out, Some(550));
        assert_eq!(realised.amount_in, Some(600));

        let without = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_input_rule(Address::from([0x22; 20]), InputRule::Word(1));
        let prepared = without
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let realised = without.execute(&prepared, Some(1)).await.unwrap();
        assert_eq!(
            realised.amount_in,
            Some(1_000),
            "a rule belongs to its own address: this router's input is its offer"
        );
    }

    /// What the rule cannot read as an input the swap could have taken is an
    /// error that names the rule, never a figure for the books.
    #[tokio::test]
    async fn an_input_the_rule_cannot_read_or_that_exceeds_the_offer_is_an_error() {
        let router = [0x11; 20];
        for (calldata, swap_return, expected) in [
            // One word only: no word 1.
            (
                vec![0x31],
                U256::from(550u64).to_be_bytes::<32>().to_vec(),
                "no word 1",
            ),
            // More than the 1 000 the payer was given.
            (
                vec![0x32],
                [U256::from(550u64), U256::from(1_001u64)]
                    .iter()
                    .flat_map(|w| w.to_be_bytes::<32>())
                    .collect(),
                "took 1001 of an offer of 1000",
            ),
            // Does not fit a u128.
            (
                vec![0x33],
                [U256::from(550u64), U256::MAX]
                    .iter()
                    .flat_map(|w| w.to_be_bytes::<32>())
                    .collect(),
                "overflowed u128",
            ),
        ] {
            let server = MockServer::start().await;
            mount_router(&server, router, &calldata, swap_return).await;
            let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
                .with_input_rule(Address::from(router), InputRule::Word(1));
            let prepared = adapter
                .prepare(&route(payload_for(router, &calldata)), &request())
                .await
                .unwrap();
            let err = adapter.execute(&prepared, Some(1)).await.unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("Word(1)") && text.contains(expected),
                "{expected}: {text}"
            );
        }
    }

    const SWAP_V3: &str = "swapV3(address,bool,uint256,uint160,uint256,uint256)";
    const SWAP_V2: &str = "swapV2(address,bool,uint256,uint256,uint256)";

    /// `PoolSwapper` has two functions at one address with two return
    /// layouts: `swapV3` returns `(amountInUsed, amountOut)`, and the pool may
    /// have stopped at the price limit with part of the offer unspent;
    /// `swapV2` returns `amountOut`, having sent the pair exactly the offer.
    /// One adapter reads each by its own rules.
    #[tokio::test]
    async fn two_functions_at_one_address_are_each_read_by_their_own_rules() {
        let server = MockServer::start().await;
        let pool_swapper = [0x11; 20];
        let (v3, v2) = (selector_of(SWAP_V3), selector_of(SWAP_V2));
        let (v3_call, v2_call) = (call_of(v3, 0x01), call_of(v2, 0x02));
        // 1 000 offered: swapV3 used 600 of it (!= its 550 out) and swapV2
        // paid out 870.
        mount_router_calls(
            &server,
            pool_swapper,
            vec![
                (v3_call.clone(), words(&[600, 550])),
                (v2_call.clone(), words(&[870])),
            ],
        )
        .await;

        let address = Address::from(pool_swapper);
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule_for(address, v3, ReturnRule::Word(1))
            .with_input_rule_for(address, v3, InputRule::Word(0))
            .with_return_rule_for(address, v2, ReturnRule::FirstWord)
            .with_input_rule_for(address, v2, InputRule::Offered);

        let swap_v3 = run(&adapter, pool_swapper, &v3_call).await.unwrap();
        assert_eq!(swap_v3.amount_out, Some(550), "swapV3's second word");
        assert_eq!(
            swap_v3.amount_in,
            Some(600),
            "swapV3's first word, not the offer"
        );
        assert!(matches!(swap_v3.outcome, Outcome::Success));

        let swap_v2 = run(&adapter, pool_swapper, &v2_call).await.unwrap();
        assert_eq!(swap_v2.amount_out, Some(870));
        assert_eq!(
            swap_v2.amount_in,
            Some(1_000),
            "swapV2 takes exactly the offer"
        );
        assert!(matches!(swap_v2.outcome, Outcome::Success));
    }

    /// The rule for a function wins over the address's rule, for that
    /// function only, whichever was set first; the address's other functions
    /// keep the address's rule.
    #[tokio::test]
    async fn a_selector_rule_beats_the_address_rule_for_its_selector_only() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let (ruled, other) = ([0xa1, 0xa2, 0xa3, 0xa4], [0xb1, 0xb2, 0xb3, 0xb4]);
        let (ruled_call, other_call) = (call_of(ruled, 1), call_of(other, 2));
        // Both functions return (111, 222) for an offer of 1 000.
        mount_router_calls(
            &server,
            router,
            vec![
                (ruled_call.clone(), words(&[111, 222])),
                (other_call.clone(), words(&[111, 222])),
            ],
        )
        .await;

        let address = Address::from(router);
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            // The function's rules are set before the address's.
            .with_return_rule_for(address, ruled, ReturnRule::Word(0))
            .with_input_rule_for(address, ruled, InputRule::Offered)
            .with_return_rule(address, ReturnRule::Word(1))
            .with_input_rule(address, InputRule::Word(1));

        let realised = run(&adapter, router, &ruled_call).await.unwrap();
        assert_eq!(realised.amount_out, Some(111), "its own return rule");
        assert_eq!(realised.amount_in, Some(1_000), "its own input rule");

        let realised = run(&adapter, router, &other_call).await.unwrap();
        assert_eq!(realised.amount_out, Some(222), "the address's return rule");
        assert_eq!(realised.amount_in, Some(222), "the address's input rule");
    }

    /// A rule for a function leaves the address's other functions, and every
    /// other address, on the defaults: a first word out, the whole offer in.
    #[tokio::test]
    async fn a_selector_rule_alone_leaves_the_rest_on_the_defaults() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let (ruled, other) = ([0xa1, 0xa2, 0xa3, 0xa4], [0xb1, 0xb2, 0xb3, 0xb4]);
        let (ruled_call, other_call) = (call_of(ruled, 1), call_of(other, 2));
        mount_router_calls(
            &server,
            router,
            vec![
                (ruled_call.clone(), words(&[111, 222])),
                (other_call.clone(), words(&[111, 222])),
            ],
        )
        .await;

        // The rules are for `ruled` at the router, and for `other` at
        // another address.
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule_for(Address::from(router), ruled, ReturnRule::Word(1))
            .with_input_rule_for(Address::from(router), ruled, InputRule::Word(1))
            .with_return_rule_for(Address::from([0x22; 20]), other, ReturnRule::Word(1))
            .with_input_rule_for(Address::from([0x22; 20]), other, InputRule::Word(1));

        let realised = run(&adapter, router, &ruled_call).await.unwrap();
        assert_eq!(
            (realised.amount_out, realised.amount_in),
            (Some(222), Some(222))
        );

        let realised = run(&adapter, router, &other_call).await.unwrap();
        assert_eq!(
            (realised.amount_out, realised.amount_in),
            (Some(111), Some(1_000)),
            "a selector's rule at another address is not this address's"
        );
    }

    /// A rule set for an address alone is what it always was: it reads every
    /// function of the address, a call whose calldata is too short to hold a
    /// selector included.
    #[tokio::test]
    async fn an_address_rule_alone_reads_every_call_to_it() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let amounts = crate::evm::rpc::tests::uint_array(&[1_000, 1_234]);
        let calls = [
            call_of([0xa1, 0xa2, 0xa3, 0xa4], 1),
            call_of([0xb1, 0xb2, 0xb3, 0xb4], 2),
            vec![0xc1, 0xc2, 0xc3],
            vec![],
        ];
        mount_router_calls(
            &server,
            router,
            calls
                .iter()
                .map(|call| (call.clone(), amounts.clone()))
                .collect(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule(Address::from(router), ReturnRule::LastOfArray)
            .with_input_rule(Address::from(router), InputRule::Word(0));
        for call in &calls {
            let realised = run(&adapter, router, call).await.unwrap();
            // The array's offset (32) is the first word: the input rule
            // reads it, and the return rule goes on to the last element.
            assert_eq!(realised.amount_out, Some(1_234), "calldata {call:?}");
            assert_eq!(realised.amount_in, Some(32), "calldata {call:?}");
        }
    }

    /// What `Word(n)` cannot read as an amount out is an error that names the
    /// rule, never a figure.
    #[tokio::test]
    async fn a_return_word_that_is_missing_or_does_not_fit_is_an_error() {
        let router = [0x11; 20];
        for (rule, swap_return, expected) in [
            // One word only: no word 1.
            (ReturnRule::Word(1), words(&[550]), "no word 1"),
            // 32 bytes of a second word, but 31 of them.
            (
                ReturnRule::Word(1),
                [words(&[550]), vec![0x01; 31]].concat(),
                "no word 1 in 63 bytes",
            ),
            (ReturnRule::Word(0), vec![], "no word 0 in 0 bytes"),
            // An index whose byte offset overflows.
            (ReturnRule::Word(usize::MAX), words(&[550, 600]), "no word"),
            // Does not fit a u128.
            (
                ReturnRule::Word(1),
                [U256::from(550u64), U256::MAX]
                    .iter()
                    .flat_map(|w| w.to_be_bytes::<32>())
                    .collect(),
                "overflowed u128",
            ),
        ] {
            let server = MockServer::start().await;
            let calldata = call_of([0xd1, 0xd2, 0xd3, 0xd4], 1);
            mount_router_calls(&server, router, vec![(calldata.clone(), swap_return)]).await;
            let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
                .with_return_rule(Address::from(router), rule);
            let err = run(&adapter, router, &calldata).await.unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains(&format!("{rule:?}")) && text.contains(expected),
                "{rule:?} {expected}: {text}"
            );
        }
    }

    /// `Word(n)` reads the `n`th word of data longer than that, whatever is
    /// after it.
    #[tokio::test]
    async fn a_return_word_is_read_from_data_of_any_length() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = call_of([0xd1, 0xd2, 0xd3, 0xd4], 1);
        mount_router_calls(
            &server,
            router,
            vec![(calldata.clone(), words(&[10, 20, 30, 40]))],
        )
        .await;
        for (n, expected) in [(0, 10), (1, 20), (2, 30), (3, 40)] {
            let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
                .with_return_rule(Address::from(router), ReturnRule::Word(n));
            let realised = run(&adapter, router, &calldata).await.unwrap();
            assert_eq!(realised.amount_out, Some(expected), "Word({n})");
        }
    }

    /// The rule for a call: its selector's, else its address's, else the
    /// default; calldata shorter than a selector has no selector's.
    #[test]
    fn a_calls_rule_is_its_selectors_then_its_addresses_then_the_default() {
        let (address, other) = (Address::from([0x11; 20]), Address::from([0x22; 20]));
        let (selector, calldata) = ([1, 2, 3, 4], [1, 2, 3, 4, 5]);
        let mut rules = Rules::<ReturnRule>::default();
        assert_eq!(rules.for_call(address, &calldata), ReturnRule::FirstWord);

        rules.set(address, Some(selector), ReturnRule::Word(1));
        assert_eq!(rules.for_call(address, &calldata), ReturnRule::Word(1));
        assert_eq!(rules.for_call(address, &[1, 2, 3, 4]), ReturnRule::Word(1));
        assert_eq!(rules.for_call(address, &[1, 2, 3]), ReturnRule::FirstWord);
        assert_eq!(
            rules.for_call(address, &[9, 2, 3, 4]),
            ReturnRule::FirstWord
        );
        assert_eq!(rules.for_call(other, &calldata), ReturnRule::FirstWord);

        rules.set(address, None, ReturnRule::LastOfArray);
        assert_eq!(rules.for_call(address, &calldata), ReturnRule::Word(1));
        assert_eq!(rules.for_call(address, &[1, 2, 3]), ReturnRule::LastOfArray);
        assert_eq!(rules.for_call(address, &[]), ReturnRule::LastOfArray);
        assert_eq!(
            rules.for_call(address, &[9, 2, 3, 4]),
            ReturnRule::LastOfArray
        );
        assert_eq!(rules.for_call(other, &calldata), ReturnRule::FirstWord);
    }

    /// A one-word answer read as an array points past the data: an error
    /// that names the rule, never a figure.
    #[tokio::test]
    async fn return_data_its_rule_cannot_decode_is_an_error() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x10];
        mount_router(
            &server,
            router,
            &calldata,
            U256::from(4_321u64).to_be_bytes::<32>().to_vec(),
        )
        .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()))
            .with_return_rule(Address::from(router), ReturnRule::LastOfArray);
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let err = adapter.execute(&prepared, Some(1)).await.unwrap_err();
        assert!(format!("{err:#}").contains("LastOfArray"), "{err:#}");
    }

    #[tokio::test]
    async fn a_return_shorter_than_one_word_is_an_error() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x0c];
        mount_router(&server, router, &calldata, vec![0x01; 31]).await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let err = adapter.execute(&prepared, Some(1)).await.unwrap_err();

        assert!(format!("{err:#}").contains("31 bytes"));
    }

    #[tokio::test]
    async fn satisfies_the_dex_contract() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x05, 0x06];
        mount_router(
            &server,
            router,
            &calldata,
            U256::from(900u64).to_be_bytes::<32>().to_vec(),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_blockNumber" })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": "0x2a" })),
            )
            .mount(&server)
            .await;

        crate::testkit::contract::dex_executor_contract(
            &EvmSimulated::new(EvmRpc::new(server.uri())),
            crate::testkit::contract::Sends::Nothing,
            crate::testkit::contract::DexContractFixture {
                route: route(payload_for(router, &calldata)),
                request: request(),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_reports_the_solidity_revert_reason() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0xff; 4];
        let sender = Address::from([0xCC; 20]);
        let router_addr = Address::from(router);

        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;
        mount_slot_probe(
            &server,
            ALLOWANCE_SELECTOR,
            allowance_slot(sender, router_addr, 0),
        )
        .await;

        let reason = "insufficient liquidity";
        let error_data_hex = format!("0x{}", hex::encode(encode_error_string(reason)));
        let expected_calldata = calldata.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let data = decode_hex(body["params"][0]["data"].as_str().unwrap()).unwrap();
                if data == expected_calldata {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": { "code": 3, "message": "execution reverted", "data": error_data_hex },
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::ZERO),
                    }))
                }
            })
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let route = route(payload_for(router, &calldata));
        let request = request();

        let prepared = adapter.prepare(&route, &request).await.unwrap();
        let realised = adapter.execute(&prepared, Some(1)).await.unwrap();

        assert_eq!(realised.amount_out, None);
        match realised.outcome {
            Outcome::Reverted { reason: got } => assert_eq!(got, reason),
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert_eq!(realised.cost, TxCost::Evm(EvmCost::default()));
        assert_eq!(realised.amount_in, None, "a revert took no input");
        assert!(
            requests(&server, "eth_estimateGas").await.is_empty(),
            "a swap that reverted has no gas to ask for"
        );
    }

    /// The block a request was made at: the second parameter of `eth_call`
    /// and `eth_estimateGas`.
    fn block_of(request: &Json) -> &str {
        request["params"][1].as_str().unwrap()
    }

    /// An `eth_getBlockByNumber` that answers `result`, whatever the tag.
    async fn mount_block(server: &MockServer, result: Json) {
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({ "method": "eth_getBlockByNumber" }),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result })),
            )
            .mount(server)
            .await;
    }

    async fn mount_block_number(server: &MockServer, number: u64) {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_blockNumber" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "jsonrpc": "2.0", "id": 1, "result": format!("0x{number:x}") }),
            ))
            .mount(server)
            .await;
    }

    /// A dry run of one swap through a fixed router, on `adapter`, at `at`.
    async fn dry_run(
        server: &MockServer,
        adapter: &EvmSimulated,
        at: Option<u64>,
    ) -> Result<Realised> {
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        mount_router(
            server,
            router,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        adapter.execute(&prepared, at).await
    }

    /// A dry run that ran reports the node's estimate for the same call: the
    /// same sender, the same block and the same overrides as the swap's own
    /// `eth_call`, with no other figure invented.
    #[tokio::test]
    async fn a_dry_run_reports_the_nodes_gas_estimate_of_the_same_call() {
        let server = MockServer::start().await;
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let realised = dry_run(&server, &adapter, Some(42)).await.unwrap();

        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(
            realised.cost,
            TxCost::Evm(EvmCost {
                gas_used: Some(ESTIMATED_GAS),
                effective_gas_price_wei: None,
                l1_fee_wei: None,
            })
        );

        let calls = eth_calls(&server).await;
        let swap = swap_call(&calls, &[0x01, 0x02, 0x03, 0x04]);
        let estimates = requests(&server, "eth_estimateGas").await;
        assert_eq!(estimates.len(), 1, "one estimate for one swap");
        assert_eq!(
            estimates[0]["params"], swap["params"],
            "the call, the block and the overrides are the swap's own"
        );
        assert_eq!(estimates[0]["params"][1], "0x2a");
        assert_eq!(
            estimates[0]["params"][0]["from"].as_str().unwrap(),
            Address::from([0xCC; 20]).to_string(),
            "the sender's swap, not an anonymous one"
        );
    }

    /// An estimate the node refuses for a call that just ran is the node
    /// disagreeing with itself, which is an error that says so and names the
    /// reason, not a swap with its gas left out.
    #[tokio::test]
    async fn an_estimate_refused_for_a_swap_that_ran_is_an_error() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        mount_probes(&server, router).await;
        mount_swap_call(
            &server,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;
        let refusal = format!("0x{}", hex::encode(encode_error_string("STF")));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_estimateGas" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": 3, "message": "execution reverted", "data": refusal },
            })))
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let err = adapter.execute(&prepared, Some(42)).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("eth_estimateGas refused a swap that eth_call ran")
                && text.contains("STF"),
            "{text}"
        );
    }

    /// A node that does not answer the estimate has said nothing about the
    /// swap's gas: an error, never a success with the gas missing.
    #[tokio::test]
    async fn an_estimate_the_node_does_not_answer_is_an_error() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        mount_probes(&server, router).await;
        mount_swap_call(
            &server,
            &calldata,
            U256::from(777u64).to_be_bytes::<32>().to_vec(),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_estimateGas" })))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let prepared = adapter
            .prepare(&route(payload_for(router, &calldata)), &request())
            .await
            .unwrap();
        let err = adapter.execute(&prepared, Some(42)).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("eth_estimateGas for the swap"),
            "{err:#}"
        );
    }

    /// Without the pending mode a run with no block given reads the latest
    /// block number once and pins every call to it, the estimate included.
    #[tokio::test]
    async fn without_the_pending_mode_every_call_is_pinned_to_the_latest_block() {
        let server = MockServer::start().await;
        mount_block_number(&server, 42).await;
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri()));
        let realised = dry_run(&server, &adapter, None).await.unwrap();

        assert_eq!(realised.at, 42);
        assert_eq!(requests(&server, "eth_blockNumber").await.len(), 1);
        assert!(requests(&server, "eth_getBlockByNumber").await.is_empty());
        let mut reads = eth_calls(&server).await;
        reads.extend(requests(&server, "eth_estimateGas").await);
        assert!(reads.len() >= 3, "two probes, the swap and its estimate");
        for read in &reads {
            assert_eq!(block_of(read), "0x2a", "{read}");
        }
    }

    /// The pending mode reads the pending block for everything: the slot
    /// probes, the swap and its estimate, so that one state answers all of
    /// them. `at` is the number the node gives its pending block.
    #[tokio::test]
    async fn the_pending_mode_reads_every_call_at_pending_and_reports_its_number() {
        let server = MockServer::start().await;
        mount_block(&server, json!({ "number": "0x65", "hash": "0x01" })).await;
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri())).with_pending_block();
        let realised = dry_run(&server, &adapter, None).await.unwrap();

        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.amount_out, Some(777));
        assert_eq!(
            realised.cost,
            TxCost::Evm(EvmCost {
                gas_used: Some(ESTIMATED_GAS),
                ..EvmCost::default()
            })
        );
        assert_eq!(realised.at, 0x65);
        assert!(requests(&server, "eth_blockNumber").await.is_empty());
        let mut reads = eth_calls(&server).await;
        reads.extend(requests(&server, "eth_estimateGas").await);
        assert!(reads.len() >= 3, "two probes, the swap and its estimate");
        for read in &reads {
            assert_eq!(block_of(read), "pending", "{read}");
        }
    }

    /// A block given to `execute` pins the run to it, pending mode or not:
    /// it is how a caller re-runs a dry run on the block it was made at.
    #[tokio::test]
    async fn a_block_given_pins_the_run_even_in_the_pending_mode() {
        let server = MockServer::start().await;
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri())).with_pending_block();
        let realised = dry_run(&server, &adapter, Some(42)).await.unwrap();

        assert_eq!(realised.at, 42);
        assert!(requests(&server, "eth_blockNumber").await.is_empty());
        assert!(requests(&server, "eth_getBlockByNumber").await.is_empty());
        let mut reads = eth_calls(&server).await;
        reads.extend(requests(&server, "eth_estimateGas").await);
        for read in &reads {
            assert_eq!(block_of(read), "0x2a", "{read}");
        }
    }

    /// A node that cannot name its pending block cannot say where the read
    /// was made: an error before anything is called, not a guess.
    #[tokio::test]
    async fn a_node_with_no_pending_block_is_an_error_before_any_call() {
        let server = MockServer::start().await;
        mount_block(&server, Json::Null).await;
        let adapter = EvmSimulated::new(EvmRpc::new(server.uri())).with_pending_block();
        let err = dry_run(&server, &adapter, None).await.unwrap_err();

        assert!(format!("{err:#}").contains("no pending block"), "{err:#}");
        assert!(eth_calls(&server).await.is_empty());
    }

    /// Everything above proves this adapter's logic against a mocked
    /// JSON-RPC server. This test proves the one part a mock can't: that
    /// the slot-probing state-override technique actually works against a
    /// *real* node's real storage layout — SPEC.md §9.2's real-RPC bar,
    /// worked toward incrementally rather than all at once.
    ///
    /// No-ops (does not fail) when `EVM_LIVE_RPC_URL` is unset, so it's
    /// silent for every contributor and CI run that hasn't opted in —
    /// exactly the "network-dependent job gated on a secret being present"
    /// `IMPLEMENTATION_PLAN.md` Phase 4 asks for. Read-only (`eth_call`,
    /// no transaction), so it needs no signer and spends nothing: probing
    /// `balanceOf`/`allowance` for an arbitrary address is safe against
    /// any real ERC-20.
    #[tokio::test]
    async fn against_real_sepolia_usdc_finds_real_balance_and_allowance_slots() {
        let Ok(rpc_url) = std::env::var("EVM_LIVE_RPC_URL") else {
            eprintln!("skipping: EVM_LIVE_RPC_URL is not set");
            return;
        };

        // Circle's official Sepolia USDC (a real, verified-deployed proxy
        // contract) and Uniswap V3's SwapRouter02 on Sepolia (used here
        // only as an arbitrary allowance spender, not actually called).
        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        // An arbitrary address — this probe never needs it to hold a real
        // balance or have granted a real allowance.
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();

        let rpc = EvmRpc::new(rpc_url);
        let block = BlockTag::Number(
            rpc.block_number()
                .await
                .expect("real Sepolia RPC should answer eth_blockNumber"),
        );
        let slots = SlotCache::default();

        let balance_index = erc20::find_balance_slot(&rpc, &slots, usdc, sender, block)
            .await
            .expect("balanceOf storage slot should be probeable on real USDC");
        let allowance_index = erc20::find_allowance_slot(&rpc, &slots, usdc, sender, router, block)
            .await
            .expect("allowance storage slot should be probeable on real USDC");

        eprintln!(
            "real Sepolia USDC @ {block:?}: balanceOf slot {balance_index}, \
             allowance slot {allowance_index}"
        );
    }

    /// Exploratory — not a permanent fixture — one real swap through a
    /// real, currently-liquid Uniswap V3 pool (USDC/WETH, 0.3% fee,
    /// verified on GeckoTerminal) via SwapRouter02's real
    /// `exactInputSingle`, gated on `EVM_LIVE_RPC_URL` the same way.
    #[tokio::test]
    async fn against_real_sepolia_executes_a_real_swap_via_swaprouter02() {
        let Ok(rpc_url) = std::env::var("EVM_LIVE_RPC_URL") else {
            eprintln!("skipping: EVM_LIVE_RPC_URL is not set");
            return;
        };

        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let weth: Address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();

        // SwapRouter02#exactInputSingle((address,address,uint24,address,uint256,uint256,uint160)) — 0x04e45aaf.
        let mut calldata = vec![0x04, 0xe4, 0x5a, 0xaf];
        calldata.extend_from_slice(&pad_address(usdc));
        calldata.extend_from_slice(&pad_address(weth));
        calldata.extend_from_slice(&U256::from(3_000u64).to_be_bytes::<32>()); // fee: 0.3%
        calldata.extend_from_slice(&pad_address(sender)); // recipient
        calldata.extend_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>()); // amountIn: 1 USDC
        calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // amountOutMinimum
        calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // sqrtPriceLimitX96

        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(&calldata);

        let route = RouteQuote {
            network: Network::evm(11_155_111),
            token_in: usdc.as_slice().to_vec(),
            token_out: weth.as_slice().to_vec(),
            amount_in: 1_000_000,
            expected_amount_out: 0,
            payload,
        };
        let request = SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: sender.as_slice().to_vec(),
            payer: Payer::Sender,
            min_amount_out: 0,
            deadline_unix_secs: 0,
            priority: PriorityBid::Policy,
        };

        let adapter = EvmSimulated::new(EvmRpc::new(rpc_url));
        let prepared = adapter.prepare(&route, &request).await.unwrap();
        let realised = adapter.execute(&prepared, None).await.unwrap();
        eprintln!("real swap result: {realised:?}");
    }

    /// Exploratory — a small real benchmark, not a permanent fixture:
    /// quote (Uniswap's own `QuoterV2`, an independent on-chain source —
    /// not this crate) vs. simulation (`EvmSimulated`, this crate) across
    /// several distinct input sizes on the same real USDC/WETH pool.
    /// Incremental progress toward §9.2's 100-distinct-input bar, not a
    /// claim of having cleared it. Both calls are `eth_call`s — read-only,
    /// nothing broadcast, no risk to the pool's thin real liquidity.
    #[tokio::test]
    async fn benchmarks_real_quote_against_real_simulation_across_several_sizes() {
        let Ok(rpc_url) = std::env::var("EVM_LIVE_RPC_URL") else {
            eprintln!("skipping: EVM_LIVE_RPC_URL is not set");
            return;
        };

        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let weth: Address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        // Uniswap's QuoterV2 on Sepolia — verified against the alternative
        // address a search initially turned up, which returned empty data
        // (not a real Quoter); this one returns a real quote.
        let quoter: Address = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
            .parse()
            .unwrap();
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();
        const FEE: u64 = 3_000; // 0.3%

        let rpc = EvmRpc::new(rpc_url);
        let adapter = EvmSimulated::new(rpc.clone());

        // 0.1, 0.5, 1, 5, 10 USDC (6 decimals).
        let amounts_in: [u64; 5] = [100_000, 500_000, 1_000_000, 5_000_000, 10_000_000];
        let mut rows = Vec::new();

        for amount_in in amounts_in {
            // Real quote: QuoterV2.quoteExactInputSingle((tokenIn, tokenOut,
            // amountIn, fee, sqrtPriceLimitX96)) — selector 0xc6a5026a.
            let mut quote_calldata = vec![0xc6, 0xa5, 0x02, 0x6a];
            quote_calldata.extend_from_slice(&pad_address(usdc));
            quote_calldata.extend_from_slice(&pad_address(weth));
            quote_calldata.extend_from_slice(&U256::from(amount_in).to_be_bytes::<32>());
            quote_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
            quote_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            let quote_bytes = rpc
                .eth_call(quoter, &quote_calldata, None, BlockTag::Latest, None)
                .await
                .expect("quoter call should succeed");
            let quoted_amount_out: u128 = first_word(&quote_bytes).unwrap().try_into().unwrap();

            // Real simulation: this crate's own EvmSimulated, through the
            // real router, exactly as a consumer would call it.
            let mut swap_calldata = vec![0x04, 0xe4, 0x5a, 0xaf];
            swap_calldata.extend_from_slice(&pad_address(usdc));
            swap_calldata.extend_from_slice(&pad_address(weth));
            swap_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
            swap_calldata.extend_from_slice(&pad_address(sender));
            swap_calldata.extend_from_slice(&U256::from(amount_in).to_be_bytes::<32>());
            swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            let mut payload = router.as_slice().to_vec();
            payload.extend_from_slice(&swap_calldata);

            let route = RouteQuote {
                network: Network::evm(11_155_111),
                token_in: usdc.as_slice().to_vec(),
                token_out: weth.as_slice().to_vec(),
                amount_in: amount_in as ChainAmount,
                expected_amount_out: quoted_amount_out,
                payload,
            };
            let request = SwapRequest {
                sender: sender.as_slice().to_vec(),
                recipient: sender.as_slice().to_vec(),
                payer: Payer::Sender,
                min_amount_out: 0,
                deadline_unix_secs: 0,
                priority: PriorityBid::Policy,
            };
            let prepared = adapter.prepare(&route, &request).await.unwrap();
            let realised = adapter.execute(&prepared, None).await.unwrap();

            rows.push((amount_in, quoted_amount_out, realised));
        }

        eprintln!(
            "\n{:>14} | {:>18} | {:>18} | {:>10} | outcome",
            "amountIn(USDC)", "quoted(wei WETH)", "simulated(wei WETH)", "diff(wei)"
        );
        for (amount_in, quoted, realised) in &rows {
            let simulated = realised.amount_out.unwrap_or(0);
            let diff = simulated as i128 - *quoted as i128;
            eprintln!(
                "{:>14} | {:>18} | {:>18} | {:>10} | {:?}",
                amount_in, quoted, simulated, diff, realised.outcome
            );
        }

        for (amount_in, quoted, realised) in &rows {
            assert_eq!(
                realised.amount_out,
                Some(*quoted),
                "quote and simulation disagreed for amountIn={amount_in}"
            );
        }
    }
}
