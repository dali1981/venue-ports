//! `EvmRpc` — the EVM chain family's JSON-RPC client, and the hex/ABI
//! helpers every EVM adapter needs (`SPEC.md` §5, §8). `EvmSimulated`,
//! `EvmLive` and `EvmLiquidity` all talk to a node through this one type
//! rather than each carrying its own copy of it.

use crate::dex::EvmCost;
use alloy_primitives::{Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// `keccak256("Error(string)")[..4]` — the standard Solidity
/// `require`/`revert("reason")` encoding.
pub const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

/// How long one JSON-RPC request may take before it is abandoned. Without
/// one, a node that accepts a connection and never answers would hang a
/// receipt poll forever instead of letting it reach its own timeout.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Which block a read is made against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTag {
    Latest,
    Pending,
    Number(u64),
}

impl BlockTag {
    fn param(self) -> String {
        match self {
            BlockTag::Latest => "latest".to_string(),
            BlockTag::Pending => "pending".to_string(),
            BlockTag::Number(n) => format!("0x{n:x}"),
        }
    }
}

/// A JSON-RPC `error` object from the node, with any Solidity revert
/// reason already decoded. Distinguished from a transport failure (a bad
/// URL, a dropped connection) so a caller can tell "the node answered no"
/// from "the node did not answer". "The node answered no" is not yet "the
/// call reverted": a node refuses a call it cannot run (a rate limit, state
/// it no longer holds) with the same kind of object, so [`RpcError::is_revert`]
/// says which it was. `EvmSimulated` turns only a revert into
/// `Outcome::Reverted`, and `EvmSender` treats a broadcast refused this way
/// as never sent.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: Option<i64>,
    pub message: String,
    /// The decoded `Error(string)` reason when the error carries one, the
    /// node's own message otherwise.
    pub reason: String,
    /// Whether the node attached revert data: `data`, or the nested
    /// `data.data` some nodes use, as a `0x` string.
    pub revert_data: bool,
}

impl RpcError {
    /// Whether this is the call reverting: the node attached revert data, or
    /// its message says the call reverted. Any other error is the node failing
    /// to run the call (a rate limit, a timeout, state it no longer holds), and
    /// says nothing about the call.
    pub fn is_revert(&self) -> bool {
        self.revert_data || self.message.to_ascii_lowercase().contains("revert")
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for RpcError {}

/// One log entry from a receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
}

impl RpcLog {
    fn from_json(log: &Value) -> Result<Self> {
        let address = log
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("log has no address: {log}"))?
            .parse()
            .context("log address")?;
        let topics = log
            .get("topics")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("log has no topics: {log}"))?
            .iter()
            .map(|t| {
                t.as_str()
                    .ok_or_else(|| anyhow!("log topic is not a string: {t}"))?
                    .parse::<B256>()
                    .context("log topic")
            })
            .collect::<Result<_>>()?;
        let data = decode_hex(
            log.get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("log has no data: {log}"))?,
        )?;
        Ok(Self {
            address,
            topics,
            data,
        })
    }
}

/// The parts of a transaction receipt the adapters read.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub success: bool,
    pub block: u64,
    pub logs: Vec<RpcLog>,
    /// What the transaction cost, as far as the receipt says: `gasUsed`,
    /// `effectiveGasPrice`, and a rollup's `l1Fee`. A field the node leaves
    /// out is `None`.
    pub cost: EvmCost,
}

#[derive(Debug, Clone)]
pub struct EvmRpc {
    url: String,
    http: reqwest::Client,
}

impl EvmRpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_request_timeout(url, DEFAULT_REQUEST_TIMEOUT)
    }

    /// As [`EvmRpc::new`], with a different limit on how long one request
    /// may take.
    pub fn with_request_timeout(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("a reqwest client with only a timeout set always builds"),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. A node `error` comes back as an [`RpcError`]
    /// inside the `anyhow::Error`; anything else that fails is transport.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("sending JSON-RPC request {method}"))?
            .json()
            .await
            .with_context(|| format!("decoding JSON-RPC response body for {method}"))?;

        if let Some(error) = response.get("error") {
            return Err(RpcError {
                code: error.get("code").and_then(Value::as_i64),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                reason: decode_revert_reason(error),
                revert_data: [error.get("data"), error.pointer("/data/data")]
                    .into_iter()
                    .flatten()
                    .find_map(Value::as_str)
                    .is_some_and(|data| data.starts_with("0x")),
            }
            .into());
        }
        response.get("result").cloned().ok_or_else(|| {
            anyhow!("JSON-RPC response to {method} had neither `result` nor `error`: {response}")
        })
    }

    /// `eth_call`. `from` matters whenever the call itself depends on
    /// `msg.sender` — a router's swap reads it to decide whose balance and
    /// allowance to pull from (a real Sepolia run of `EvmSimulated` reverted
    /// with Uniswap's `"STF"` before `from` was passed, because `msg.sender`
    /// defaulted to the zero address while the state overrides were written
    /// for the route's sender). It is irrelevant for a pure view call.
    pub async fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        from: Option<Address>,
        block: BlockTag,
        overrides: Option<&Value>,
    ) -> Result<Vec<u8>> {
        let result = self
            .call("eth_call", call_params(to, data, from, block, overrides))
            .await?;
        decode_hex(
            result
                .as_str()
                .ok_or_else(|| anyhow!("eth_call result was not a hex string: {result}"))?,
        )
    }

    /// `eth_estimateGas` of the call [`EvmRpc::eth_call`] would make with the
    /// same arguments: the node's figure for the gas the call needs, at
    /// `block`, with `overrides` applied. A call the node would revert is an
    /// [`RpcError`], as for `eth_call`.
    pub async fn eth_estimate_gas(
        &self,
        to: Address,
        data: &[u8],
        from: Option<Address>,
        block: BlockTag,
        overrides: Option<&Value>,
    ) -> Result<u64> {
        self.call_u64(
            "eth_estimateGas",
            call_params(to, data, from, block, overrides),
        )
        .await
    }

    pub async fn block_number(&self) -> Result<u64> {
        self.call_u64("eth_blockNumber", json!([])).await
    }

    /// The number of the block `block` names: a number is itself, `latest` is
    /// `eth_blockNumber`, and `pending` is the number the node gives its
    /// pending block (on Base, the preconfirmed one). A node with no pending
    /// block is an error: the caller asked where a read was made, and a
    /// guess would be an answer to something else.
    pub async fn block_number_at(&self, block: BlockTag) -> Result<u64> {
        match block {
            BlockTag::Number(number) => Ok(number),
            BlockTag::Latest => self.block_number().await,
            BlockTag::Pending => {
                let header = self
                    .call("eth_getBlockByNumber", json!([block.param(), false]))
                    .await?;
                parse_hex_u64(
                    header
                        .get("number")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("the node has no pending block: {header}"))?,
                )
            }
        }
    }

    pub async fn chain_id(&self) -> Result<u64> {
        self.call_u64("eth_chainId", json!([])).await
    }

    pub async fn client_version(&self) -> Result<String> {
        let result = self.call("web3_clientVersion", json!([])).await?;
        result
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("web3_clientVersion result was not a string: {result}"))
    }

    pub async fn transaction_count(&self, address: Address, block: BlockTag) -> Result<u64> {
        self.call_u64(
            "eth_getTransactionCount",
            json!([address.to_string(), block.param()]),
        )
        .await
    }

    pub async fn balance(&self, address: Address) -> Result<U256> {
        let result = self
            .call("eth_getBalance", json!([address.to_string(), "latest"]))
            .await?;
        parse_hex_u256(
            result
                .as_str()
                .ok_or_else(|| anyhow!("eth_getBalance result was not a hex string: {result}"))?,
        )
    }

    pub async fn code(&self, address: Address) -> Result<Vec<u8>> {
        let result = self
            .call("eth_getCode", json!([address.to_string(), "latest"]))
            .await?;
        decode_hex(
            result
                .as_str()
                .ok_or_else(|| anyhow!("eth_getCode result was not a hex string: {result}"))?,
        )
    }

    /// The latest block's base fee, in wei.
    pub async fn base_fee(&self) -> Result<u128> {
        let block = self
            .call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        parse_hex_u128(
            block
                .get("baseFeePerGas")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!("latest block has no baseFeePerGas — is this chain EIP-1559-enabled?")
                })?,
        )
    }

    /// The node's suggested priority fee, in wei. Not every node implements
    /// `eth_maxPriorityFeePerGas`; callers fall back on their own figure.
    pub async fn max_priority_fee(&self) -> Result<u128> {
        let result = self.call("eth_maxPriorityFeePerGas", json!([])).await?;
        parse_hex_u128(result.as_str().ok_or_else(|| {
            anyhow!("eth_maxPriorityFeePerGas result was not a hex string: {result}")
        })?)
    }

    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        value: U256,
    ) -> Result<u64> {
        let call = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": hex_data(data),
            "value": format!("0x{value:x}"),
        });
        self.call_u64("eth_estimateGas", json!([call])).await
    }

    pub async fn send_raw_transaction(&self, raw: &[u8]) -> Result<()> {
        self.call("eth_sendRawTransaction", json!([hex_data(raw)]))
            .await?;
        Ok(())
    }

    /// `eth_sendTransaction`: the node signs. Only a fork sender uses this,
    /// from an account the fork impersonates.
    pub async fn send_transaction(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        value: U256,
        gas: u64,
    ) -> Result<B256> {
        let tx = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": hex_data(data),
            "value": format!("0x{value:x}"),
            "gas": format!("0x{gas:x}"),
        });
        let result = self.call("eth_sendTransaction", json!([tx])).await?;
        result
            .as_str()
            .ok_or_else(|| anyhow!("eth_sendTransaction result was not a hex string: {result}"))?
            .parse()
            .context("eth_sendTransaction result")
    }

    /// `None` while the node has no receipt for `tx_hash`.
    pub async fn transaction_receipt(&self, tx_hash: B256) -> Result<Option<Receipt>> {
        let receipt = self
            .call("eth_getTransactionReceipt", json!([tx_hash.to_string()]))
            .await?;
        if receipt.is_null() {
            return Ok(None);
        }
        let success = receipt
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("receipt for {tx_hash} has no status: {receipt}"))?
            == "0x1";
        let block = parse_hex_u64(
            receipt
                .get("blockNumber")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("receipt for {tx_hash} has no blockNumber: {receipt}"))?,
        )?;
        let logs = receipt
            .get("logs")
            .and_then(Value::as_array)
            .map(|logs| logs.iter().map(RpcLog::from_json).collect::<Result<_>>())
            .transpose()?
            .unwrap_or_default();
        let quantity = |name: &str| {
            receipt
                .get(name)
                .and_then(Value::as_str)
                .and_then(|hex| u128::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
        };
        let cost = EvmCost {
            gas_used: quantity("gasUsed").and_then(|gas| u64::try_from(gas).ok()),
            effective_gas_price_wei: quantity("effectiveGasPrice"),
            l1_fee_wei: quantity("l1Fee"),
        };
        Ok(Some(Receipt {
            success,
            block,
            logs,
            cost,
        }))
    }

    /// Whether the node knows `tx_hash` at all — pending or mined.
    pub async fn knows_transaction(&self, tx_hash: B256) -> Result<bool> {
        let tx = self
            .call("eth_getTransactionByHash", json!([tx_hash.to_string()]))
            .await?;
        Ok(!tx.is_null())
    }

    /// Replays a reverted call with `eth_call` at the block it was mined
    /// in, to recover the Solidity revert reason a receipt never carries.
    /// Best-effort: if the replay cannot produce a reason (state moved, or
    /// the node disagrees), says so rather than guessing.
    pub async fn revert_reason(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        value: U256,
        block: u64,
    ) -> String {
        let call = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": hex_data(data),
            "value": format!("0x{value:x}"),
        });
        match self
            .call("eth_call", json!([call, BlockTag::Number(block).param()]))
            .await
        {
            Err(err) => match err.downcast_ref::<RpcError>() {
                Some(rpc) => rpc.reason.clone(),
                None => format!("transaction reverted (replaying it failed: {err:#})"),
            },
            Ok(_) => {
                "transaction reverted (replaying the call at its block did not reproduce a revert)"
                    .to_string()
            }
        }
    }

    async fn call_u64(&self, method: &str, params: Value) -> Result<u64> {
        let result = self.call(method, params).await?;
        parse_hex_u64(
            result
                .as_str()
                .ok_or_else(|| anyhow!("{method} result was not a hex string: {result}"))?,
        )
    }
}

/// The parameters of `eth_call` and `eth_estimateGas`, which take the same
/// three: the call, the block, and (when given) the state overrides.
fn call_params(
    to: Address,
    data: &[u8],
    from: Option<Address>,
    block: BlockTag,
    overrides: Option<&Value>,
) -> Value {
    let mut call = json!({ "to": to.to_string(), "data": hex_data(data) });
    if let Some(from) = from {
        call["from"] = json!(from.to_string());
    }
    match overrides {
        Some(overrides) => json!([call, block.param(), overrides]),
        None => json!([call, block.param()]),
    }
}

pub fn hex_data(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

pub fn decode_hex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).context("decoding hex string")
}

pub fn parse_hex_u64(s: &str) -> Result<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .with_context(|| format!("could not parse hex u64 {s}"))
}

pub fn parse_hex_u128(s: &str) -> Result<u128> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .with_context(|| format!("could not parse hex u128 {s}"))
}

pub fn parse_hex_u256(s: &str) -> Result<U256> {
    U256::from_str_radix(s.trim_start_matches("0x"), 16)
        .with_context(|| format!("could not parse hex u256 {s}"))
}

pub fn address_from_slice(bytes: &[u8]) -> Result<Address> {
    let array: [u8; 20] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected a 20-byte EVM address, got {} bytes", bytes.len()))?;
    Ok(Address::from(array))
}

pub fn pad_address(address: Address) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[12..32].copy_from_slice(address.as_slice());
    buf
}

/// Formats a `U256` as 32 bytes of big-endian hex `DATA` — the shape a
/// node expects for a storage value and returns for a `uint256` return
/// value. Unlike a JSON-RPC `QUANTITY` (which drops leading zeros, so zero
/// is legally `"0x0"`), `DATA` here must stay a fixed 32 bytes or a node
/// can reject or misinterpret it.
pub fn format_u256(value: U256) -> String {
    format!("0x{}", hex::encode(value.to_be_bytes::<32>()))
}

/// The first 32-byte word of ABI-encoded return data, as a `uint256`.
/// Fewer than 32 bytes is an error; anything after the first word is
/// ignored, never a panic.
pub fn first_word(data: &[u8]) -> Result<U256> {
    let word = data.get(..32).ok_or_else(|| {
        anyhow!(
            "expected at least one 32-byte word of return data, got {} bytes",
            data.len()
        )
    })?;
    Ok(U256::from_be_slice(word))
}

/// The last element of ABI-encoded return data that is one `uint256[]`: the
/// head's single word is the array's offset, then its length, then its
/// elements. A router that returns every hop's amount (Uniswap v2's and
/// Aerodrome's `swapExactTokensForTokens`) puts the amount out last. An
/// offset or a length that points past the data, or an empty array, is an
/// error, never a panic.
pub fn last_of_uint_array(data: &[u8]) -> Result<U256> {
    let word = |at: usize| -> Result<U256> {
        let end = at.checked_add(32).filter(|end| *end <= data.len()).ok_or_else(|| {
            anyhow!(
                "expected a 32-byte word at byte {at} of the uint256[] return data, which is {} bytes",
                data.len()
            )
        })?;
        Ok(U256::from_be_slice(&data[at..end]))
    };
    let offset: usize = word(0)?
        .try_into()
        .map_err(|_| anyhow!("the uint256[] offset does not fit the return data"))?;
    let len: usize = word(offset)?
        .try_into()
        .map_err(|_| anyhow!("the uint256[] length does not fit the return data"))?;
    if len == 0 {
        bail!("the router returned an empty uint256[]: it names no amount out");
    }
    let last = len
        .checked_mul(32)
        .and_then(|n| n.checked_add(offset))
        .ok_or_else(|| anyhow!("the uint256[] length {len} does not fit the return data"))?;
    word(last)
}

/// Best-effort decode of a Solidity revert reason from a JSON-RPC error's
/// `data` field: the standard `Error(string)` `require`/`revert` encoding
/// when present. Any other revert data (a custom error, a `Panic(uint256)`)
/// follows the node's message as `<message> (revert data 0x…)`, so the
/// caller that knows the contract can decode it. With no data, the node's
/// own message.
pub fn decode_revert_reason(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("execution reverted");
    let data = error
        .get("data")
        .and_then(Value::as_str)
        .or_else(|| error.pointer("/data/data").and_then(Value::as_str));

    if let Some(data) = data {
        if let Ok(bytes) = decode_hex(data) {
            if bytes.len() >= 4 && bytes[..4] == ERROR_STRING_SELECTOR {
                if let Some(reason) = decode_abi_string(&bytes[4..]) {
                    return reason;
                }
            }
            if bytes.len() >= 4 {
                return format!("{message} (revert data {})", hex_data(&bytes));
            }
        }
    }
    message.to_string()
}

/// Decodes a lone ABI-encoded `string` (the standard `Error(string)`
/// payload shape): a 32-byte offset, a 32-byte length, then the UTF-8
/// bytes.
pub fn decode_abi_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 64 {
        return None;
    }
    let len: usize = U256::from_be_slice(&bytes[32..64]).try_into().ok()?;
    let start: usize = 64;
    let end = start.checked_add(len)?;
    bytes
        .get(start..end)
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

/// `Error(string)` ABI-encoded — what a node returns as revert data for
/// `require(cond, reason)`. Used by tests that stand in for a node.
#[cfg(test)]
pub(crate) fn encode_error_string(reason: &str) -> Vec<u8> {
    let mut data = ERROR_STRING_SELECTOR.to_vec();
    data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(reason.len() as u64).to_be_bytes::<32>());
    let mut padded = reason.as_bytes().to_vec();
    padded.resize(padded.len().div_ceil(32) * 32, 0);
    data.extend_from_slice(&padded);
    data
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn first_word_reads_a_two_word_return_and_refuses_a_short_one() {
        // KyberSwap's MetaAggregationRouterV2.swap returns
        // (uint256 returnAmount, uint256 gasUsed).
        let mut data = U256::from(777u64).to_be_bytes::<32>().to_vec();
        data.extend_from_slice(&U256::from(150_000u64).to_be_bytes::<32>());
        assert_eq!(first_word(&data).unwrap(), U256::from(777u64));

        assert!(first_word(&[0u8; 31]).is_err());
    }

    /// `[amountIn, amountOut]` as Aerodrome's router returns it for one hop.
    pub(crate) fn uint_array(items: &[u64]) -> Vec<u8> {
        let mut data = U256::from(32u64).to_be_bytes::<32>().to_vec();
        data.extend_from_slice(&U256::from(items.len()).to_be_bytes::<32>());
        for item in items {
            data.extend_from_slice(&U256::from(*item).to_be_bytes::<32>());
        }
        data
    }

    #[test]
    fn last_of_uint_array_reads_the_amount_out_and_refuses_what_points_past_the_data() {
        assert_eq!(
            last_of_uint_array(&uint_array(&[20_000_000, 25_100_000])).unwrap(),
            U256::from(25_100_000u64)
        );
        assert_eq!(
            last_of_uint_array(&uint_array(&[1, 2, 3])).unwrap(),
            U256::from(3u64)
        );

        let empty = last_of_uint_array(&uint_array(&[])).unwrap_err();
        assert!(empty.to_string().contains("empty"), "{empty}");
        let mut short = uint_array(&[1, 2]);
        short.truncate(short.len() - 1);
        assert!(last_of_uint_array(&short).is_err());
        let mut far = uint_array(&[1, 2]);
        far[..32].copy_from_slice(&U256::MAX.to_be_bytes::<32>());
        assert!(last_of_uint_array(&far).is_err());
        let mut long = uint_array(&[1, 2]);
        long[32..64].copy_from_slice(&U256::from(u64::MAX).to_be_bytes::<32>());
        assert!(last_of_uint_array(&long).is_err());
        assert!(last_of_uint_array(&[0u8; 16]).is_err());
    }

    #[test]
    fn decodes_an_error_string_revert_reason() {
        let error = json!({
            "code": 3,
            "message": "execution reverted",
            "data": hex_data(&encode_error_string("Price slippage check")),
        });
        assert_eq!(decode_revert_reason(&error), "Price slippage check");

        let bare = json!({ "code": -32000, "message": "nonce too low" });
        assert_eq!(decode_revert_reason(&bare), "nonce too low");
    }

    /// A custom error is not decoded here, which does not know the contract:
    /// its data follows the node's message, for the caller that does.
    #[test]
    fn keeps_a_custom_errors_data_beside_the_message() {
        // `TooLittle(uint256,uint256)` with (5, 6).
        let mut revert = vec![0x61, 0x8b, 0xc7, 0xc9];
        revert.extend_from_slice(&U256::from(5u64).to_be_bytes::<32>());
        revert.extend_from_slice(&U256::from(6u64).to_be_bytes::<32>());
        let error =
            json!({ "code": 3, "message": "execution reverted", "data": hex_data(&revert) });
        assert_eq!(
            decode_revert_reason(&error),
            format!("execution reverted (revert data {})", hex_data(&revert))
        );

        let nested =
            json!({ "code": -32000, "message": "execution reverted", "data": { "data": "0x" } });
        assert_eq!(decode_revert_reason(&nested), "execution reverted");
    }
}
