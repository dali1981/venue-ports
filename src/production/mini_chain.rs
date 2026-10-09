//! A stand-in for a Base node with an Aerodrome-style pool on it, for the
//! self-tests of LV2b (`lv2b_chain`): a stateful JSON-RPC `wiremock` `Respond`,
//! as `MiniExchange` is for LV2a.
//!
//! It keeps one account (the run's signer) with a native balance and a balance of
//! each of two tokens, a pool that swaps one for the other at a fixed rate less a
//! fee, a router that pulls the input against an allowance, and a chain that
//! mines **one block for each transaction** it is sent. It decodes what it is
//! sent (`alloy_consensus::TxEnvelope::decode_2718`, as `src/evm/tx.rs`'s tests do)
//! and applies it: an `approve` sets an allowance; a
//! `swapExactTokensForTokens` moves tokens, writes the `Transfer` logs of the
//! swap among others that are not its own, charges the fee, and consumes the
//! nonce. It keeps a snapshot of the account at every block, so a balance read
//! "at the block before" and "at the block" differs by what the block did.
//!
//! **What it answers is shaped as a node's replies are, and the numbers are the
//! mock's own**; nothing here says what Base answers. It checks what a node
//! checks of a transaction: the chain id, the nonce, the allowance and the
//! balance, and the router's `amountOutMin`.
//!
//! Each [`Knobs`] field is one way for the chain to disagree with the adapter, or
//! to behave as a chain does at its worst: a swap that is mined late, never, or
//! reverted; a receipt that reads differently the second time; a balance that
//! moves by more than the swap; a nonce that skips.

use super::lv2b_chain::abi::{Pool, Router, Token};
use crate::evm::erc20::{
    transfer_topic, ALLOWANCE_SELECTOR, APPROVE_SELECTOR, BALANCE_OF_SELECTOR,
};
use crate::evm::rpc::{decode_hex, encode_error_string, format_u256, hex_data};
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::SolCall;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wiremock::{Request, Respond, ResponseTemplate};

pub(super) const CHAIN_ID: u64 = 8453;
pub(super) const ROUTER: Address = Address::new([0x12; 20]);
pub(super) const POOL: Address = Address::new([0x34; 20]);
pub(super) const FACTORY: Address = Address::new([0x56; 20]);
/// The USD-pegged token the run starts holding (6 decimals).
pub(super) const TOKEN_IN: Address = Address::new([0xa1; 20]);
/// The token the pool prices (18 decimals).
pub(super) const TOKEN_OUT: Address = Address::new([0xb2; 20]);
/// A token and a recipient that appear in a swap's logs and are not its own.
pub(super) const NOISE_TOKEN: Address = Address::new([0xc3; 20]);
pub(super) const OTHER: Address = Address::new([0x77; 20]);

pub(super) const FIRST_BLOCK: u64 = 1_000;
/// 0.006 gwei: what a Base block's base fee is of the order of.
pub(super) const BASE_FEE: u128 = 6_000_000;
pub(super) const PRIORITY_FEE: u128 = 1_000_000;
pub(super) const APPROVE_GAS: u64 = 46_000;
pub(super) const SWAP_GAS: u64 = 150_000;
pub(super) const REVERT_GAS: u64 = 61_000;
pub(super) const APPROVE_L1_FEE: u128 = 1_000_000_000_000;
pub(super) const SWAP_L1_FEE: u128 = 2_000_000_000_000;
/// Units of the output token for one unit of the input token: 1 AERO costs
/// 1.25 USD, the input has 6 decimals and the output 18.
pub(super) const RATE: u128 = 800_000_000_000;
pub(super) const START_NATIVE: u128 = 1_000_000_000_000_000_000;
pub(super) const START_TOKEN_IN: u128 = 1_000_000_000;

/// A hook on a node's method, called with its name as it is asked.
pub(super) type Hook = Arc<dyn Fn(&str) + Send + Sync>;
/// A change to a receipt as the node serves it, given the signer's address.
pub(super) type Tweak = Arc<dyn Fn(&mut Value, Address) + Send + Sync>;

/// What makes the chain disagree with the adapter, or behave badly.
#[derive(Clone)]
pub(super) struct Knobs {
    pub(super) client_version: String,
    pub(super) chain_id: u64,
    /// The pool's fee, in basis points, taken from what a swap delivers.
    pub(super) fee_bps: u128,
    /// How many polls of a transaction's receipt find nothing before it is
    /// found.
    pub(super) receipt_polls_before_found: u32,
    /// How many `eth_blockNumber` polls after a receipt is first served still
    /// report the block before it: a *preconfirmed* receipt, not yet sealed.
    pub(super) seal_lag_polls: u32,
    /// Other transactions in a swap's block, before it and after it.
    pub(super) neighbours_ahead: u64,
    pub(super) neighbours_behind: u64,
    /// Empty blocks mined before a swap's: a landing that is late.
    pub(super) empty_blocks_before: u64,
    /// A swap is accepted and never mined.
    pub(super) never_mines_swap: bool,
    /// The node's estimate of a swap reverts.
    pub(super) estimate_reverts: bool,
    /// The pool's `stable()` reverts.
    pub(super) stable_reverts: bool,
    /// The swaps (1-based, in the order they are sent) the router reverts.
    pub(super) forced_revert: BTreeSet<usize>,
    /// The swaps whose delivery is this many basis points under the quote; the
    /// router reverts one that is under its `amountOutMin`.
    pub(super) slip_bps: BTreeMap<usize, u128>,
    /// Input the router sends back to the payer after a swap.
    pub(super) refund_in: u128,
    /// Wei charged to the signer beyond a swap's fee.
    pub(super) native_extra_charge: u128,
    /// Input tokens taken from the signer beyond the swap's amount.
    pub(super) token_leak: u128,
    /// After a swap the signer's nonce is one higher than it should be.
    pub(super) nonce_skip: bool,
    /// Applied to a swap's receipt every time it is served after the first.
    pub(super) evidence_tweak: Option<Tweak>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            client_version: "reth/v1.1.0-mock".to_string(),
            chain_id: CHAIN_ID,
            fee_bps: 30,
            receipt_polls_before_found: 1,
            seal_lag_polls: 3,
            neighbours_ahead: 0,
            neighbours_behind: 0,
            empty_blocks_before: 0,
            never_mines_swap: false,
            estimate_reverts: false,
            stable_reverts: false,
            forced_revert: BTreeSet::new(),
            slip_bps: BTreeMap::new(),
            refund_in: 0,
            native_extra_charge: 0,
            token_leak: 0,
            nonce_skip: false,
            evidence_tweak: None,
        }
    }
}

/// The signer's holdings at a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Account {
    pub(super) native: u128,
    pub(super) token_in: u128,
    pub(super) token_out: u128,
}

impl Account {
    fn token(&self, token: Address) -> u128 {
        if token == TOKEN_IN {
            self.token_in
        } else if token == TOKEN_OUT {
            self.token_out
        } else {
            0
        }
    }

    fn token_mut(&mut self, token: Address) -> Option<&mut u128> {
        if token == TOKEN_IN {
            Some(&mut self.token_in)
        } else if token == TOKEN_OUT {
            Some(&mut self.token_out)
        } else {
            None
        }
    }
}

/// A swap the chain mined, for a test to read.
#[derive(Clone, Debug)]
pub(super) struct SwapRecord {
    /// 1-based, in the order the swaps were sent.
    pub(super) number: usize,
    pub(super) hash: B256,
    pub(super) block: u64,
    pub(super) from: Address,
    pub(super) to: Address,
    pub(super) amount_in: u128,
    /// What the pool delivered; zero for a swap that reverted.
    pub(super) amount_out: u128,
    pub(super) reverted: Option<String>,
    pub(super) gas_used: u64,
    pub(super) effective_gas_price: u128,
    pub(super) l1_fee: u128,
    pub(super) nonce: u64,
}

struct Mined {
    block: u64,
    receipt: Value,
    swap: bool,
    reverted: Option<String>,
}

/// What a receipt is built from.
struct ReceiptSpec {
    hash: B256,
    block: u64,
    to: Address,
    status: bool,
    gas: u64,
    price: u128,
    l1: u128,
    logs: Vec<Value>,
}

struct Fault {
    code: i64,
    message: String,
    data: Option<String>,
}

fn refusal(code: i64, message: impl Into<String>) -> Fault {
    Fault {
        code,
        message: message.into(),
        data: None,
    }
}

/// A call that reverts with `Error(reason)`, as a node answers it.
fn reverted(reason: &str) -> Fault {
    Fault {
        code: 3,
        message: "execution reverted".to_string(),
        data: Some(hex_data(&encode_error_string(reason))),
    }
}

struct Chain {
    knobs: Knobs,
    signer: Address,
    /// The last block mined, and the last the node *reports* (a receipt that is
    /// not yet sealed leaves it behind).
    block: u64,
    visible: u64,
    /// The highest block whose receipt has been served: the node reports it
    /// sealed `seal_lag_polls` block-number polls later.
    served: u64,
    seal_left: Option<u32>,
    nonce: u64,
    now: Account,
    history: BTreeMap<u64, Account>,
    allowance: HashMap<Address, U256>,
    mined: HashMap<B256, Mined>,
    mempool: HashSet<B256>,
    reads: HashMap<B256, u32>,
    counts: HashMap<u64, u64>,
    swaps_seen: usize,
    swaps: Vec<SwapRecord>,
}

/// An address as a 32-byte log topic.
pub(super) fn word(address: Address) -> String {
    format!("0x{}{}", "00".repeat(12), hex::encode(address))
}

fn quantity(value: impl Into<u128>) -> String {
    format!("0x{:x}", value.into())
}

fn block_of(tag: &Value) -> Option<u64> {
    tag.as_str()
        .and_then(|text| text.strip_prefix("0x"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
}

impl Chain {
    fn new(signer: Address, knobs: Knobs) -> Self {
        let start = Account {
            native: START_NATIVE,
            token_in: START_TOKEN_IN,
            token_out: 0,
        };
        Self {
            knobs,
            signer,
            block: FIRST_BLOCK,
            visible: FIRST_BLOCK,
            served: FIRST_BLOCK,
            seal_left: None,
            nonce: 0,
            now: start,
            history: BTreeMap::from([(FIRST_BLOCK, start)]),
            allowance: HashMap::new(),
            mined: HashMap::new(),
            mempool: HashSet::new(),
            reads: HashMap::new(),
            counts: HashMap::new(),
            swaps_seen: 0,
            swaps: Vec::new(),
        }
    }

    /// The account as of a block named by a JSON-RPC tag: `latest` and `pending`
    /// are now; a number is its snapshot (the last one at or before it).
    fn account_at(&self, tag: &Value) -> Account {
        match block_of(tag) {
            Some(number) if number < self.block => self
                .history
                .range(..=number)
                .next_back()
                .or_else(|| self.history.iter().next())
                .map_or(self.now, |(_, account)| *account),
            _ => self.now,
        }
    }

    /// `amount` of `from` against the pool, less its fee.
    fn quote(&self, from: Address, amount: U256) -> U256 {
        let net = U256::from(10_000 - self.knobs.fee_bps);
        let bps = U256::from(10_000u64);
        if from == TOKEN_IN {
            amount * U256::from(RATE) * net / bps
        } else {
            amount / U256::from(RATE) * net / bps
        }
    }

    /// What the node reports as the latest block. A receipt that has been
    /// served starts a countdown after which the block it is in is sealed: the
    /// node does not report a block that no receipt has been served from.
    fn block_number(&mut self) -> u64 {
        if let Some(left) = self.seal_left {
            if left == 0 {
                self.visible = self.served;
                self.seal_left = None;
            } else {
                self.seal_left = Some(left - 1);
            }
        }
        self.visible
    }

    fn mine_empty_block(&mut self) {
        self.block += 1;
        self.history.insert(self.block, self.now);
        self.counts.insert(self.block, 0);
    }

    fn charge_fee(&mut self, gas: u64, price: u128, l1: u128, extra: u128) {
        let fee = u128::from(gas) * price + l1 + extra;
        self.now.native = self.now.native.saturating_sub(fee);
    }

    fn receipt(&self, spec: ReceiptSpec) -> Value {
        let ReceiptSpec {
            hash,
            block,
            to,
            status,
            gas,
            price,
            l1,
            logs,
        } = spec;
        let ahead = self.knobs.neighbours_ahead;
        json!({
            "status": if status { "0x1" } else { "0x0" },
            "blockNumber": format!("0x{block:x}"),
            "transactionIndex": format!("0x{ahead:x}"),
            "transactionHash": hash.to_string(),
            "from": self.signer.to_string(),
            "to": to.to_string(),
            "gasUsed": quantity(gas),
            "cumulativeGasUsed": quantity(u128::from(gas) + u128::from(ahead) * 21_000),
            "effectiveGasPrice": quantity(price),
            "l1Fee": quantity(l1),
            "logs": logs,
        })
    }

    fn transfer_log(
        token: Address,
        from: Address,
        to: Address,
        value: u128,
        index: u64,
        block: u64,
        hash: B256,
    ) -> Value {
        json!({
            "address": token.to_string(),
            "topics": [transfer_topic().to_string(), word(from), word(to)],
            "data": format_u256(U256::from(value)),
            "logIndex": format!("0x{index:x}"),
            "blockNumber": format!("0x{block:x}"),
            "transactionHash": hash.to_string(),
        })
    }

    fn send_raw(&mut self, raw: &[u8]) -> Result<B256, Fault> {
        let tx = TxEnvelope::decode_2718(&mut &raw[..])
            .map_err(|err| refusal(-32602, format!("invalid raw transaction: {err}")))?;
        let hash = *tx.tx_hash();
        if tx.chain_id() != Some(self.knobs.chain_id) {
            return Err(refusal(
                -32000,
                format!("invalid chain id {:?}", tx.chain_id()),
            ));
        }
        if tx.nonce() < self.nonce {
            return Err(refusal(
                -32000,
                format!(
                    "nonce too low: next nonce {}, tx nonce {}",
                    self.nonce,
                    tx.nonce()
                ),
            ));
        }
        if tx.nonce() > self.nonce {
            return Err(refusal(
                -32000,
                format!(
                    "nonce too high: next nonce {}, tx nonce {}",
                    self.nonce,
                    tx.nonce()
                ),
            ));
        }
        let to = tx
            .to()
            .ok_or_else(|| refusal(-32000, "a contract creation"))?;
        let input = tx.input().to_vec();
        let tip = tx
            .max_priority_fee_per_gas()
            .unwrap_or(0)
            .min(tx.max_fee_per_gas().saturating_sub(BASE_FEE));
        let price = BASE_FEE + tip;
        let selector = input.get(..4).unwrap_or_default();
        if (to == TOKEN_IN || to == TOKEN_OUT) && selector == APPROVE_SELECTOR {
            let amount = input
                .get(36..68)
                .map(U256::from_be_slice)
                .ok_or_else(|| refusal(-32000, "a short approve"))?;
            self.mine_approve(hash, to, amount, price);
        } else if to == ROUTER && selector == Router::swapExactTokensForTokensCall::SELECTOR {
            self.swaps_seen += 1;
            if self.knobs.never_mines_swap {
                self.mempool.insert(hash);
            } else {
                self.mine_swap(hash, &input, price)?;
            }
        } else {
            return Err(refusal(-32000, format!("the mock has no contract at {to}")));
        }
        Ok(hash)
    }

    fn mine_approve(&mut self, hash: B256, token: Address, amount: U256, price: u128) {
        self.block += 1;
        let block = self.block;
        self.allowance.insert(token, amount);
        self.charge_fee(APPROVE_GAS, price, APPROVE_L1_FEE, 0);
        self.history.insert(block, self.now);
        self.nonce += 1;
        let approval = json!({
            "address": token.to_string(),
            "topics": [
                keccak256(b"Approval(address,address,uint256)").to_string(),
                word(self.signer),
                word(ROUTER),
            ],
            "data": format_u256(amount),
            "logIndex": "0x0",
            "blockNumber": format!("0x{block:x}"),
            "transactionHash": hash.to_string(),
        });
        let receipt = self.receipt(ReceiptSpec {
            hash,
            block,
            to: token,
            status: true,
            gas: APPROVE_GAS,
            price,
            l1: APPROVE_L1_FEE,
            logs: vec![approval],
        });
        self.counts.insert(block, 1);
        self.mined.insert(
            hash,
            Mined {
                block,
                receipt,
                swap: false,
                reverted: None,
            },
        );
    }

    fn mine_swap(&mut self, hash: B256, input: &[u8], price: u128) -> Result<(), Fault> {
        let call = Router::swapExactTokensForTokensCall::abi_decode(input)
            .map_err(|err| refusal(-32000, format!("the router cannot decode the swap: {err}")))?;
        let route = call
            .routes
            .first()
            .ok_or_else(|| refusal(-32000, "a swap with no route"))?;
        let (from, to) = (route.from, route.to);
        let amount_in = u128::try_from(call.amountIn)
            .map_err(|_| refusal(-32000, "the mock holds amounts in a u128"))?;
        let number = self.swaps_seen;

        for _ in 0..self.knobs.empty_blocks_before {
            self.mine_empty_block();
        }
        self.block += 1;
        let block = self.block;
        let nonce = self.nonce;
        let (ahead, behind) = (self.knobs.neighbours_ahead, self.knobs.neighbours_behind);
        let first_log = ahead * 4;

        // The router's own checks, in the order a router makes them.
        let allowed = self.allowance.get(&from).copied().unwrap_or(U256::ZERO);
        let quoted = self.quote(from, call.amountIn);
        let delivered = match self.knobs.slip_bps.get(&number) {
            Some(slip) => quoted * U256::from(10_000 - slip) / U256::from(10_000u64),
            None => quoted,
        };
        let failure: Option<&str> = if self.knobs.forced_revert.contains(&number) {
            Some("Pool: LOCKED")
        } else if allowed < call.amountIn || self.now.token(from) < amount_in {
            Some("STF")
        } else if delivered < call.amountOutMin {
            Some("Router: INSUFFICIENT_OUTPUT_AMOUNT")
        } else {
            None
        };

        let (record_out, logs, gas, l1, extra) = match failure {
            Some(_) => (0, Vec::new(), REVERT_GAS, SWAP_L1_FEE, 0),
            None => {
                let out = u128::try_from(delivered)
                    .map_err(|_| refusal(-32000, "the mock holds amounts in a u128"))?;
                let refund = self.knobs.refund_in.min(amount_in);
                let spend = amount_in - refund + self.knobs.token_leak;
                if let Some(held) = self.now.token_mut(from) {
                    *held = held.saturating_sub(spend);
                }
                if let Some(held) = self.now.token_mut(to) {
                    *held += out;
                }
                self.allowance
                    .insert(from, allowed.saturating_sub(call.amountIn));
                let mut logs = vec![Self::transfer_log(
                    from,
                    self.signer,
                    POOL,
                    amount_in,
                    first_log,
                    block,
                    hash,
                )];
                let mut index = first_log + 1;
                if refund > 0 {
                    logs.push(Self::transfer_log(
                        from,
                        POOL,
                        self.signer,
                        refund,
                        index,
                        block,
                        hash,
                    ));
                    index += 1;
                }
                // Transfers that are not the swap's: of the output to another
                // account, and of another token to the signer.
                logs.push(Self::transfer_log(to, POOL, OTHER, 77, index, block, hash));
                logs.push(Self::transfer_log(
                    NOISE_TOKEN,
                    POOL,
                    self.signer,
                    99,
                    index + 1,
                    block,
                    hash,
                ));
                logs.push(Self::transfer_log(
                    to,
                    POOL,
                    self.signer,
                    out,
                    index + 2,
                    block,
                    hash,
                ));
                (
                    out,
                    logs,
                    SWAP_GAS,
                    SWAP_L1_FEE,
                    self.knobs.native_extra_charge,
                )
            }
        };
        self.charge_fee(gas, price, l1, extra);
        self.history.insert(block, self.now);
        self.nonce += 1 + u64::from(self.knobs.nonce_skip);
        let receipt = self.receipt(ReceiptSpec {
            hash,
            block,
            to: ROUTER,
            status: failure.is_none(),
            gas,
            price,
            l1,
            logs,
        });
        self.counts.insert(block, ahead + 1 + behind);
        self.swaps.push(SwapRecord {
            number,
            hash,
            block,
            from,
            to,
            amount_in,
            amount_out: record_out,
            reverted: failure.map(str::to_string),
            gas_used: gas,
            effective_gas_price: price,
            l1_fee: l1,
            nonce,
        });
        self.mined.insert(
            hash,
            Mined {
                block,
                receipt,
                swap: true,
                reverted: failure.map(str::to_string),
            },
        );
        Ok(())
    }

    fn receipt_of(&mut self, hash: B256) -> Value {
        let Some(mined) = self.mined.get(&hash) else {
            return Value::Null;
        };
        let reads = self.reads.entry(hash).or_insert(0);
        *reads += 1;
        if *reads <= self.knobs.receipt_polls_before_found {
            return Value::Null;
        }
        let first = *reads == self.knobs.receipt_polls_before_found + 1;
        let mut receipt = mined.receipt.clone();
        if first {
            if self.visible < mined.block {
                self.served = self.served.max(mined.block);
                self.seal_left = Some(self.knobs.seal_lag_polls);
            }
        } else if mined.swap {
            if let Some(tweak) = &self.knobs.evidence_tweak {
                tweak(&mut receipt, self.signer);
            }
        }
        receipt
    }

    fn eth_call(&mut self, call: &Value, tag: &Value) -> Result<Value, Fault> {
        let to: Address = call["to"]
            .as_str()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| refusal(-32602, "an eth_call with no `to`"))?;
        let data = call["data"]
            .as_str()
            .or_else(|| call["input"].as_str())
            .map(decode_hex)
            .transpose()
            .map_err(|err| refusal(-32602, format!("bad call data: {err}")))?
            .unwrap_or_default();
        let selector = data.get(..4).unwrap_or_default();
        let hex = |bytes: Vec<u8>| Ok(json!(hex_data(&bytes)));

        if to == ROUTER && selector == Router::getAmountsOutCall::SELECTOR {
            let decoded = Router::getAmountsOutCall::abi_decode(&data)
                .map_err(|err| refusal(-32000, format!("getAmountsOut: {err}")))?;
            let from = decoded
                .routes
                .first()
                .ok_or_else(|| reverted("INVALID_PATH"))?
                .from;
            let out = self.quote(from, decoded.amountIn);
            return hex(Router::getAmountsOutCall::abi_encode_returns(&vec![
                decoded.amountIn,
                out,
            ]));
        }
        if to == ROUTER && selector == Router::swapExactTokensForTokensCall::SELECTOR {
            // A replay of a swap at the block it was mined in: the node says
            // what the swap said.
            let block = block_of(tag);
            return match self
                .mined
                .values()
                .find(|mined| mined.swap && Some(mined.block) == block)
                .and_then(|mined| mined.reverted.clone())
            {
                Some(reason) => Err(reverted(&reason)),
                None => hex(Vec::new()),
            };
        }
        if to == POOL && selector == Pool::stableCall::SELECTOR {
            if self.knobs.stable_reverts {
                return Err(reverted("Pool: not a pool"));
            }
            return hex(Pool::stableCall::abi_encode_returns(&false));
        }
        if to == POOL && selector == Pool::factoryCall::SELECTOR {
            return hex(Pool::factoryCall::abi_encode_returns(&FACTORY));
        }
        if (to == TOKEN_IN || to == TOKEN_OUT) && selector == Token::decimalsCall::SELECTOR {
            return hex(Token::decimalsCall::abi_encode_returns(
                &(if to == TOKEN_IN { 6u8 } else { 18u8 }),
            ));
        }
        if selector == BALANCE_OF_SELECTOR {
            let holder = data.get(16..36).map(Address::from_slice);
            let held = if holder == Some(self.signer) {
                self.account_at(tag).token(to)
            } else {
                0
            };
            return Ok(json!(format_u256(U256::from(held))));
        }
        if selector == ALLOWANCE_SELECTOR {
            let held = self.allowance.get(&to).copied().unwrap_or(U256::ZERO);
            return Ok(json!(format_u256(held)));
        }
        Err(refusal(
            -32000,
            format!("the mock does not answer an eth_call to {to}"),
        ))
    }

    fn handle(&mut self, method: &str, params: &Value) -> Result<Value, Fault> {
        match method {
            "eth_chainId" => Ok(json!(quantity(u128::from(self.knobs.chain_id)))),
            "web3_clientVersion" => Ok(json!(self.knobs.client_version)),
            "eth_blockNumber" => Ok(json!(quantity(u128::from(self.block_number())))),
            "eth_getBlockByNumber" => {
                let number = if params[0] == "pending" {
                    self.block + 1
                } else {
                    self.block
                };
                Ok(json!({
                    "number": quantity(u128::from(number)),
                    "baseFeePerGas": quantity(BASE_FEE),
                }))
            }
            "eth_maxPriorityFeePerGas" => Ok(json!(quantity(PRIORITY_FEE))),
            "eth_getTransactionCount" => Ok(json!(quantity(u128::from(self.nonce)))),
            "eth_getBalance" => Ok(json!(quantity(self.account_at(&params[1]).native))),
            "eth_estimateGas" => {
                let selector = params[0]["data"]
                    .as_str()
                    .and_then(|data| decode_hex(data).ok())
                    .unwrap_or_default();
                if selector.get(..4) == Some(&Router::swapExactTokensForTokensCall::SELECTOR[..]) {
                    if self.knobs.estimate_reverts {
                        return Err(reverted("Router: EXPIRED"));
                    }
                    Ok(json!(quantity(u128::from(SWAP_GAS))))
                } else {
                    Ok(json!(quantity(u128::from(APPROVE_GAS))))
                }
            }
            "eth_sendRawTransaction" => {
                let raw = params[0]
                    .as_str()
                    .map(decode_hex)
                    .transpose()
                    .map_err(|err| refusal(-32602, format!("bad raw transaction: {err}")))?
                    .unwrap_or_default();
                self.send_raw(&raw).map(|hash| json!(hash.to_string()))
            }
            "eth_getTransactionReceipt" => {
                let hash: B256 = params[0]
                    .as_str()
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| refusal(-32602, "a receipt for no hash"))?;
                Ok(self.receipt_of(hash))
            }
            "eth_getTransactionByHash" => {
                let hash: B256 = params[0]
                    .as_str()
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| refusal(-32602, "a transaction for no hash"))?;
                Ok(
                    if self.mined.contains_key(&hash) || self.mempool.contains(&hash) {
                        json!({"hash": hash.to_string()})
                    } else {
                        Value::Null
                    },
                )
            }
            "eth_getBlockTransactionCountByNumber" => {
                let block = if params[0] == "latest" {
                    Some(self.block)
                } else {
                    block_of(&params[0])
                };
                Ok(match block {
                    Some(number) if number <= self.block => {
                        json!(quantity(u128::from(
                            self.counts.get(&number).copied().unwrap_or(0)
                        )))
                    }
                    _ => Value::Null,
                })
            }
            "eth_call" => self.eth_call(&params[0], &params[1]),
            other => Err(refusal(
                -32601,
                format!("the method {other} does not exist/is not available"),
            )),
        }
    }
}

/// The node, as a `wiremock` responder. Clones share one chain.
#[derive(Clone)]
pub(super) struct MiniChain {
    chain: Arc<Mutex<Chain>>,
    asked: Arc<Mutex<Vec<(String, Value)>>>,
    hook: Arc<Mutex<Option<Hook>>>,
}

impl MiniChain {
    pub(super) fn new(signer: Address, knobs: Knobs) -> Self {
        Self {
            chain: Arc::new(Mutex::new(Chain::new(signer, knobs))),
            asked: Arc::new(Mutex::new(Vec::new())),
            hook: Arc::new(Mutex::new(None)),
        }
    }

    /// `hook` is called with each method's name as it is asked, before it is
    /// answered.
    pub(super) fn on_method(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *self.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    pub(super) fn asked(&self) -> Vec<(String, Value)> {
        self.asked.lock().unwrap().clone()
    }

    pub(super) fn times_asked(&self, method: &str) -> usize {
        self.asked()
            .iter()
            .filter(|(asked, _)| asked == method)
            .count()
    }

    /// The signer's holdings now.
    pub(super) fn account(&self) -> Account {
        self.chain.lock().unwrap().now
    }

    /// The signer's holdings as of a block.
    pub(super) fn account_at(&self, block: u64) -> Account {
        self.chain
            .lock()
            .unwrap()
            .account_at(&json!(format!("0x{block:x}")))
    }

    pub(super) fn nonce(&self) -> u64 {
        self.chain.lock().unwrap().nonce
    }

    pub(super) fn swaps(&self) -> Vec<SwapRecord> {
        self.chain.lock().unwrap().swaps.clone()
    }

    /// The router's allowance from the signer for `token`.
    pub(super) fn allowance(&self, token: Address) -> U256 {
        self.chain
            .lock()
            .unwrap()
            .allowance
            .get(&token)
            .copied()
            .unwrap_or(U256::ZERO)
    }
}

impl Respond for MiniChain {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = body["method"].as_str().unwrap_or_default().to_string();
        let params = body["params"].clone();
        self.asked
            .lock()
            .unwrap()
            .push((method.clone(), params.clone()));
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(&method);
        }
        let reply = self.chain.lock().unwrap().handle(&method, &params);
        let envelope = match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": body["id"], "result": result}),
            Err(fault) => {
                let mut error = json!({"code": fault.code, "message": fault.message});
                if let Some(data) = fault.data {
                    error["data"] = json!(data);
                }
                json!({"jsonrpc": "2.0", "id": body["id"], "error": error})
            }
        };
        ResponseTemplate::new(200).set_body_json(envelope)
    }
}
