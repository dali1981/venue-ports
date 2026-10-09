//! The exact checks of LV2b, and the references of a swap
//! (`specs/V7-production-validation.md`, "LV2b"), as functions of what the chain
//! answered.
//!
//! The checks compare what the adapter reported (`Realised`) with **an
//! independent decoding of the raw `eth_getTransactionReceipt` JSON**, written
//! here with string and integer arithmetic and none of the adapter's code, so
//! that a mistake in the adapter's reading cannot be the same mistake in the
//! check. Each check takes the answers it compares and nothing else, so each is
//! tested alone: once on answers that agree, and once for each field changed.
//!
//! | # | Check |
//! |---|---|
//! | X1 | `Realised.amount_out` is the sum of the output token's `Transfer`s to the recipient |
//! | X2 | `Realised.amount_in` is the input token's `Transfer`s out of the payer, less any back to it |
//! | X3 | `cost` is the receipt's `gasUsed`, `effectiveGasPrice` and `l1Fee` |
//! | X4 | the signer's native balance fell by `gasUsed × effectiveGasPrice + l1Fee` between the block before and the block |
//! | X5 | the signer's token balances changed by the two amounts |
//! | X6 | `at` is the inclusion block, `tx_ref` the transaction's hash, the nonce advanced by one |

use crate::dex::{Outcome, Realised, TxCost};
use crate::Provenance;
use alloy_primitives::{Address, B256, U256};
use rust_decimal::Decimal;
use serde_json::{json, Value};

/// `keccak256("Transfer(address,address,uint256)")`, written out so that the
/// check does not borrow the adapter's.
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// One ERC-20 `Transfer` log of a receipt, as the raw JSON says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawTransfer {
    pub(crate) log_index: u64,
    pub(crate) token: Address,
    pub(crate) from: Address,
    pub(crate) to: Address,
    pub(crate) value: U256,
}

fn hex_u256(text: &str, what: &str) -> Result<U256, String> {
    let digits = text.trim_start_matches("0x");
    if digits.is_empty() {
        return Ok(U256::ZERO);
    }
    U256::from_str_radix(digits, 16).map_err(|err| format!("{what} {text:?} is not hex: {err}"))
}

fn hex_u64(text: &str, what: &str) -> Result<u64, String> {
    u64::try_from(hex_u256(text, what)?).map_err(|_| format!("{what} {text:?} does not fit a u64"))
}

/// An address from the last twenty bytes of a 32-byte topic.
fn topic_address(topic: &str) -> Result<Address, String> {
    let digits = topic.trim_start_matches("0x");
    if digits.len() != 64 {
        return Err(format!("the topic {topic:?} is not 32 bytes"));
    }
    format!("0x{}", &digits[24..])
        .parse()
        .map_err(|err| format!("the topic {topic:?} holds no address: {err}"))
}

/// Every ERC-20 `Transfer` in the receipt's logs (three topics, the first the
/// `Transfer` signature), in order, read from the raw JSON alone. An ERC-721
/// `Transfer` has four topics and is not one.
pub(crate) fn transfers_of(receipt: &Value) -> Result<Vec<RawTransfer>, String> {
    let logs = receipt
        .get("logs")
        .and_then(Value::as_array)
        .ok_or_else(|| "the receipt has no logs array".to_string())?;
    let mut found = Vec::new();
    for log in logs {
        let topics = log
            .get("topics")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("a log has no topics: {log}"))?;
        let first = topics.first().and_then(Value::as_str).unwrap_or_default();
        if topics.len() != 3 || !first.eq_ignore_ascii_case(TRANSFER_TOPIC) {
            continue;
        }
        let text = |value: Option<&Value>, what: &str| {
            value
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("a Transfer log has no {what}: {log}"))
        };
        let data = text(log.get("data"), "data")?;
        if data.trim_start_matches("0x").len() < 64 {
            return Err(format!("a Transfer log's data is under 32 bytes: {log}"));
        }
        found.push(RawTransfer {
            log_index: hex_u64(&text(log.get("logIndex"), "logIndex")?, "logIndex")?,
            token: text(log.get("address"), "address")?
                .parse()
                .map_err(|err| format!("a log's address is not an address: {err}"))?,
            from: topic_address(&text(topics.get(1), "sender topic")?)?,
            to: topic_address(&text(topics.get(2), "recipient topic")?)?,
            value: hex_u256(&data.trim_start_matches("0x")[..64], "a Transfer's value")?,
        });
    }
    Ok(found)
}

/// A hex quantity of the receipt: `None` when the node leaves it out.
fn quantity(receipt: &Value, name: &str) -> Result<Option<u128>, String> {
    match receipt.get(name).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("the receipt's {name} is not a string: {value}"))?;
            u128::try_from(hex_u256(text, name)?)
                .map(Some)
                .map_err(|_| format!("the receipt's {name} {text:?} does not fit a u128"))
        }
    }
}

fn sum(mut values: impl Iterator<Item = U256>, what: &str) -> Result<U256, String> {
    values.try_fold(U256::ZERO, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| format!("{what} overflows"))
    })
}

// --- X1, X2 -------------------------------------------------------------------

/// X1: `Realised.amount_out` is the sum of the `Transfer`s of `token_out` to
/// `recipient` in the raw receipt.
pub(crate) fn x1_amount_out(
    realised: &Realised,
    receipt: &Value,
    token_out: Address,
    recipient: Address,
) -> Result<(), String> {
    let received = sum(
        transfers_of(receipt)?
            .into_iter()
            .filter(|t| t.token == token_out && t.to == recipient)
            .map(|t| t.value),
        "the output token's transfers",
    )?;
    match realised.amount_out {
        Some(reported) if U256::from(reported) == received => Ok(()),
        Some(reported) => Err(format!(
            "Realised.amount_out is {reported}; the receipt's Transfers of {token_out} to \
             {recipient} sum to {received}"
        )),
        None => Err("Realised has no amount_out for a swap that landed".to_string()),
    }
}

/// X2: `Realised.amount_in` is the `Transfer`s of `token_in` out of `payer` in
/// the raw receipt, less any back to it.
pub(crate) fn x2_amount_in(
    realised: &Realised,
    receipt: &Value,
    token_in: Address,
    payer: Address,
) -> Result<(), String> {
    let transfers = transfers_of(receipt)?;
    let paid = sum(
        transfers
            .iter()
            .filter(|t| t.token == token_in && t.from == payer)
            .map(|t| t.value),
        "the input token's transfers out",
    )?;
    let refunded = sum(
        transfers
            .iter()
            .filter(|t| t.token == token_in && t.to == payer)
            .map(|t| t.value),
        "the input token's transfers back",
    )?;
    let taken = paid.checked_sub(refunded).ok_or_else(|| {
        format!("{refunded} of {token_in} came back to {payer}, more than the {paid} that left it")
    })?;
    match realised.amount_in {
        Some(reported) if U256::from(reported) == taken => Ok(()),
        Some(reported) => Err(format!(
            "Realised.amount_in is {reported}; the receipt's Transfers of {token_in} out of \
             {payer} come to {taken} ({paid} out, {refunded} back)"
        )),
        None => Err("Realised has no amount_in for a swap that landed".to_string()),
    }
}

// --- X3, X4 -------------------------------------------------------------------

/// X3: the cost `Realised` reports is the raw receipt's `gasUsed`,
/// `effectiveGasPrice` and `l1Fee`, a field the node leaves out being `None` on
/// both sides.
pub(crate) fn x3_cost(realised: &Realised, receipt: &Value) -> Result<(), String> {
    let TxCost::Evm(cost) = &realised.cost else {
        return Err("Realised carries a cost that is not an EVM one".to_string());
    };
    let gas = quantity(receipt, "gasUsed")?;
    let price = quantity(receipt, "effectiveGasPrice")?;
    let l1 = quantity(receipt, "l1Fee")?;
    let mut wrong = Vec::new();
    if cost.gas_used.map(u128::from) != gas {
        wrong.push(format!(
            "gas_used {:?} against gasUsed {gas:?}",
            cost.gas_used
        ));
    }
    if cost.effective_gas_price_wei != price {
        wrong.push(format!(
            "effective_gas_price_wei {:?} against effectiveGasPrice {price:?}",
            cost.effective_gas_price_wei
        ));
    }
    if cost.l1_fee_wei != l1 {
        wrong.push(format!(
            "l1_fee_wei {:?} against l1Fee {l1:?}",
            cost.l1_fee_wei
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the cost is not the receipt's: {}",
            wrong.join("; ")
        ))
    }
}

/// What the receipt says the transaction cost, in wei: `gasUsed ×
/// effectiveGasPrice + l1Fee`. An `l1Fee` the receipt does not carry is a chain
/// with none: nothing. A `gasUsed` or `effectiveGasPrice` it does not carry is
/// an error, not a free transaction.
pub(crate) fn fee_of(receipt: &Value) -> Result<u128, String> {
    let gas = quantity(receipt, "gasUsed")?
        .ok_or_else(|| "the receipt has no gasUsed, so its fee cannot be computed".to_string())?;
    let price = quantity(receipt, "effectiveGasPrice")?.ok_or_else(|| {
        "the receipt has no effectiveGasPrice, so its fee cannot be computed".to_string()
    })?;
    let l1 = quantity(receipt, "l1Fee")?.unwrap_or(0);
    gas.checked_mul(price)
        .and_then(|execution| execution.checked_add(l1))
        .ok_or_else(|| format!("{gas} gas at {price} wei plus {l1} of L1 fee overflows"))
}

/// X4: the signer's native balance, read at the block before the swap's and at
/// the swap's block, fell by exactly the receipt's fee. This is the first
/// measurement of a real L1 fee.
pub(crate) fn x4_native_change(
    receipt: &Value,
    native_before: u128,
    native_after: u128,
) -> Result<(), String> {
    let fee = fee_of(receipt)?;
    match native_before.checked_sub(native_after) {
        Some(fell) if fell == fee => Ok(()),
        Some(fell) => Err(format!(
            "the signer's native balance fell by {fell} wei ({native_before} to {native_after}); \
             the receipt's fee is {fee} wei"
        )),
        None => Err(format!(
            "the signer's native balance rose from {native_before} to {native_after} across the swap's block"
        )),
    }
}

// --- X5 -----------------------------------------------------------------------

/// X5: the signer's balance of `token_in` fell by `amount_in` and its balance of
/// `token_out` rose by `amount_out`, between the block before the swap's and the
/// swap's block. For a swap that reverted both amounts are zero.
pub(crate) fn x5_token_changes(
    amount_in: u128,
    amount_out: u128,
    token_in: (U256, U256),
    token_out: (U256, U256),
) -> Result<(), String> {
    let mut wrong = Vec::new();
    match token_in.0.checked_sub(token_in.1) {
        Some(fell) if fell == U256::from(amount_in) => {}
        _ => wrong.push(format!(
            "the input token went from {} to {}, the swap took {amount_in}",
            token_in.0, token_in.1
        )),
    }
    match token_out.1.checked_sub(token_out.0) {
        Some(rose) if rose == U256::from(amount_out) => {}
        _ => wrong.push(format!(
            "the output token went from {} to {}, the swap delivered {amount_out}",
            token_out.0, token_out.1
        )),
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the signer's token balances did not change by the swap: {}",
            wrong.join("; ")
        ))
    }
}

// --- X6 -----------------------------------------------------------------------

/// X6: `at` is the block the raw receipt names, `tx_ref` is its transaction's
/// hash, the outcome agrees with its status, the provenance is `Landed`, and the
/// signer's nonce (read at `latest` before and after) advanced by one.
pub(crate) fn x6_landing(
    realised: &Realised,
    receipt: &Value,
    nonce_before: u64,
    nonce_after: u64,
) -> Result<(), String> {
    let mut wrong = Vec::new();
    let block = receipt
        .get("blockNumber")
        .and_then(Value::as_str)
        .ok_or_else(|| "the receipt has no blockNumber".to_string())
        .and_then(|text| hex_u64(text, "blockNumber"))?;
    if realised.at != block {
        wrong.push(format!(
            "Realised.at is {}, the receipt's block is {block}",
            realised.at
        ));
    }
    let hash = receipt
        .get("transactionHash")
        .and_then(Value::as_str)
        .ok_or_else(|| "the receipt has no transactionHash".to_string())?;
    let reported = realised
        .tx_ref
        .as_deref()
        .map(|bytes| format!("0x{}", hex::encode(bytes)));
    if reported.as_deref().map(str::to_ascii_lowercase) != Some(hash.to_ascii_lowercase()) {
        wrong.push(format!(
            "Realised.tx_ref is {reported:?}, the receipt's transaction is {hash}"
        ));
    }
    let status = receipt.get("status").and_then(Value::as_str);
    match (&realised.outcome, status) {
        (Outcome::Success, Some("0x1")) | (Outcome::Reverted { .. }, Some("0x0")) => {}
        (outcome, status) => wrong.push(format!(
            "the outcome is {outcome:?}, the receipt's status is {status:?}"
        )),
    }
    if realised.provenance != Provenance::Landed {
        wrong.push(format!(
            "the provenance is {:?}, not Landed",
            realised.provenance
        ));
    }
    if nonce_after != nonce_before.saturating_add(1) {
        wrong.push(format!(
            "the signer's nonce went from {nonce_before} to {nonce_after}, not up by one"
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

// --- the references of a swap -------------------------------------------------

/// The router's quote just before a swap was sent.
#[derive(Debug, Clone)]
pub(crate) struct Quote {
    /// `getAmountsOut` for the swap's amount, read at `pending`.
    pub(crate) amount_out: U256,
    /// The latest block number when it was read.
    pub(crate) block: u64,
    /// The local clock (ns since the epoch) when the read was made.
    pub(crate) local_ns: u128,
}

/// Everything known of a swap, for its `references`. A field that does not exist
/// is `None` and is written `null`, never zero.
pub(crate) struct SwapFacts<'a> {
    pub(crate) quote: Option<&'a Quote>,
    pub(crate) calldata: &'a [u8],
    pub(crate) router: Address,
    pub(crate) token_in: Address,
    pub(crate) token_out: Address,
    pub(crate) amount_in: U256,
    /// The local clock before the signed transaction left.
    pub(crate) sent_ns: Option<u128>,
    pub(crate) tx_hash: Option<B256>,
    /// The first time `eth_getTransactionReceipt` returned the transaction.
    pub(crate) first_seen_ns: Option<u128>,
    /// The first time `eth_blockNumber` was at or past the transaction's block.
    pub(crate) sealed_ns: Option<u128>,
    pub(crate) realised: Option<&'a Realised>,
    /// The raw receipt, as the node sent it.
    pub(crate) receipt: Option<&'a Value>,
    pub(crate) block_tx_count: Option<u64>,
}

fn ns(value: u128) -> Value {
    u64::try_from(value).map_or(Value::Null, |value| json!(value))
}

fn address(value: Address) -> String {
    value.to_string()
}

/// `(amount_out − quote) / quote` in basis points, to two decimals, signed: how
/// far the swap's output was from the quote it was sent against. `None` where it
/// cannot be computed (a quote of zero, an amount past what the arithmetic
/// holds).
fn bps_against_quote(amount_out: u128, quote: U256) -> Option<Decimal> {
    let quote = i128::try_from(u128::try_from(quote).ok()?).ok()?;
    if quote == 0 {
        return None;
    }
    let out = i128::try_from(amount_out).ok()?;
    // Hundredths of a basis point, rounded toward zero.
    let hundredths = out.checked_sub(quote)?.checked_mul(100_000_000)? / quote;
    Decimal::try_from_i128_with_scale(hundredths, 4).ok()
}

/// The `references` of a swap (`specs/V7-production-validation.md`, "LV2b"):
/// what is needed to run the same swap again at another block, when it left and
/// was seen and sealed, where it landed, what it cost and what came of it, beside
/// the quote it was sent against.
///
/// These are the references of the consumer's terms 2 (the quote to the top of the
/// landing block), 3 (the top of the landing block to what was returned) and 6
/// (gas and the L1 fee). Term 1 needs a decision block's simulation, and this tier
/// has no decision: `decision_simulation` is `null`.
pub(crate) fn swap_references(facts: &SwapFacts<'_>) -> Value {
    let realised = facts.realised;
    let cost = realised.and_then(|r| match &r.cost {
        TxCost::Evm(cost) => Some(cost),
        TxCost::Solana(_) => None,
    });
    let transfer_log_indices = facts.receipt.map(|receipt| {
        transfers_of(receipt).map_or(Value::Null, |found| {
            json!(found.iter().map(|t| t.log_index).collect::<Vec<_>>())
        })
    });
    let transaction_index = facts
        .receipt
        .and_then(|receipt| receipt.get("transactionIndex"))
        .and_then(Value::as_str)
        .and_then(|text| hex_u64(text, "transactionIndex").ok());
    let bps = realised
        .and_then(|r| r.amount_out)
        .zip(facts.quote)
        .and_then(|(out, quote)| bps_against_quote(out, quote.amount_out));
    json!({
        "quote_pre_send": facts.quote.map(|quote| json!({
            "amount_out": quote.amount_out.to_string(),
            "block": quote.block,
            "local_ns": ns(quote.local_ns),
        })),
        "calldata": format!("0x{}", hex::encode(facts.calldata)),
        "router": address(facts.router),
        "token_in": address(facts.token_in),
        "token_out": address(facts.token_out),
        "amount_in": facts.amount_in.to_string(),
        "sent_ns": facts.sent_ns.map_or(Value::Null, ns),
        "tx_hash": facts.tx_hash.map(|hash| hash.to_string()),
        "first_seen_ns": facts.first_seen_ns.map_or(Value::Null, ns),
        "sealed_ns": facts.sealed_ns.map_or(Value::Null, ns),
        "inclusion_block": realised.map(|r| r.at),
        "transaction_index": transaction_index,
        "block_tx_count": facts.block_tx_count,
        "transfer_log_indices": transfer_log_indices,
        "amount_in_taken": realised.and_then(|r| r.amount_in).map(|v| v.to_string()),
        "amount_out": realised.and_then(|r| r.amount_out).map(|v| v.to_string()),
        "gas_used": cost.and_then(|c| c.gas_used),
        "effective_gas_price_wei": cost.and_then(|c| c.effective_gas_price_wei).map(|v| v.to_string()),
        "l1_fee_wei": cost.and_then(|c| c.l1_fee_wei).map(|v| v.to_string()),
        "amount_out_vs_quote_bps": bps.map(|bps| bps.to_string()),
        "revert": realised.and_then(|r| match &r.outcome {
            Outcome::Reverted { reason } => Some(reason.clone()),
            _ => None,
        }),
        "decision_simulation": Value::Null,
    })
}

/// The names of the fields every swap's `references` has, present or `null`.
pub(crate) const SWAP_REFERENCE_FIELDS: [&str; 22] = [
    "quote_pre_send",
    "calldata",
    "router",
    "token_in",
    "token_out",
    "amount_in",
    "sent_ns",
    "tx_hash",
    "first_seen_ns",
    "sealed_ns",
    "inclusion_block",
    "transaction_index",
    "block_tx_count",
    "transfer_log_indices",
    "amount_in_taken",
    "amount_out",
    "gas_used",
    "effective_gas_price_wei",
    "l1_fee_wei",
    "amount_out_vs_quote_bps",
    "revert",
    "decision_simulation",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::EvmCost;

    const SIGNER: Address = Address::new([0x51; 20]);
    const POOL: Address = Address::new([0x90; 20]);
    const OTHER: Address = Address::new([0x77; 20]);
    const TOKEN_IN: Address = Address::new([0xa1; 20]);
    const TOKEN_OUT: Address = Address::new([0xb2; 20]);
    const NOISE: Address = Address::new([0xc3; 20]);
    const HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

    fn word(address: Address) -> String {
        format!("0x{}{}", "00".repeat(12), hex::encode(address))
    }

    fn data(value: u128) -> String {
        format!("0x{:064x}", value)
    }

    fn log(token: Address, from: Address, to: Address, value: u128, index: u64) -> Value {
        json!({
            "address": token.to_string(),
            "topics": [TRANSFER_TOPIC, word(from), word(to)],
            "data": data(value),
            "logIndex": format!("0x{index:x}"),
        })
    }

    /// A receipt of a swap of 10_000_000 in (6 decimals) for 9_980 000 000 000
    /// 000 000 000 out, with a refund of 500, an unrelated Transfer of the
    /// output token to someone else and one of another token to the signer.
    fn receipt() -> Value {
        json!({
            "status": "0x1",
            "blockNumber": "0x3e8",
            "transactionIndex": "0x7",
            "transactionHash": HASH,
            "gasUsed": "0x249f0",
            "effectiveGasPrice": "0x5b8d80",
            "l1Fee": "0x1d1a94a2000",
            "logs": [
                log(TOKEN_IN, SIGNER, POOL, 10_000_000, 3),
                log(TOKEN_IN, POOL, SIGNER, 500, 4),
                log(TOKEN_OUT, POOL, OTHER, 77, 5),
                log(NOISE, POOL, SIGNER, 99, 6),
                log(TOKEN_OUT, POOL, SIGNER, 9_980_000_000_000_000_000, 7),
                {"address": TOKEN_OUT.to_string(), "topics": [TRANSFER_TOPIC, word(POOL), word(SIGNER), data(1)], "data": "0x", "logIndex": "0x8"},
            ],
        })
    }

    fn realised() -> Realised {
        Realised {
            amount_out: Some(9_980_000_000_000_000_000),
            amount_in: Some(9_999_500),
            outcome: Outcome::Success,
            cost: TxCost::Evm(EvmCost {
                gas_used: Some(0x249f0),
                effective_gas_price_wei: Some(0x5b8d80),
                l1_fee_wei: Some(0x1d1a94a2000),
            }),
            at: 1000,
            provenance: Provenance::Landed,
            tx_ref: Some(vec![0x11; 32]),
        }
    }

    fn fee() -> u128 {
        0x249f0u128 * 0x5b8d80 + 0x1d1a94a2000
    }

    // --- the decoding ---

    #[test]
    fn the_decoding_reads_each_transfer_and_skips_what_is_not_one() {
        let found = transfers_of(&receipt()).unwrap();
        // The ERC-721-shaped log (four topics) is skipped.
        assert_eq!(found.len(), 5);
        assert_eq!(found[0].from, SIGNER);
        assert_eq!(found[0].to, POOL);
        assert_eq!(found[0].value, U256::from(10_000_000));
        assert_eq!(found[4].value, U256::from(9_980_000_000_000_000_000u128));
        let indices: Vec<u64> = found.iter().map(|t| t.log_index).collect();
        assert_eq!(indices, [3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_receipt_that_cannot_be_decoded_is_an_error_naming_what_is_wrong() {
        assert!(transfers_of(&json!({})).unwrap_err().contains("no logs"));
        let mut broken = receipt();
        broken["logs"][0]["data"] = json!("0x01");
        assert!(transfers_of(&broken)
            .unwrap_err()
            .contains("under 32 bytes"));
        let mut no_index = receipt();
        no_index["logs"][0]
            .as_object_mut()
            .unwrap()
            .remove("logIndex");
        assert!(transfers_of(&no_index).unwrap_err().contains("logIndex"));
        let mut bad_topic = receipt();
        bad_topic["logs"][0]["topics"][1] = json!("0x1234");
        assert!(transfers_of(&bad_topic)
            .unwrap_err()
            .contains("not 32 bytes"));
    }

    // --- X1, X2 ---

    #[test]
    fn x1_the_amount_out_is_the_transfers_to_the_recipient_and_no_others() {
        assert_eq!(
            x1_amount_out(&realised(), &receipt(), TOKEN_OUT, SIGNER),
            Ok(())
        );
        // The Transfer to someone else is not counted: counting it would be wrong by 77.
        let mut counted_all = realised();
        counted_all.amount_out = Some(9_980_000_000_000_000_077);
        assert!(x1_amount_out(&counted_all, &receipt(), TOKEN_OUT, SIGNER).is_err());
        let mut none = realised();
        none.amount_out = None;
        assert!(x1_amount_out(&none, &receipt(), TOKEN_OUT, SIGNER).is_err());
        // A different recipient, a different token.
        assert!(x1_amount_out(&realised(), &receipt(), TOKEN_OUT, OTHER).is_err());
        assert!(x1_amount_out(&realised(), &receipt(), NOISE, SIGNER).is_err());
    }

    #[test]
    fn x1_two_transfers_to_the_recipient_are_added() {
        let mut two = receipt();
        two["logs"]
            .as_array_mut()
            .unwrap()
            .push(log(TOKEN_OUT, POOL, SIGNER, 20, 9));
        let mut reported = realised();
        reported.amount_out = Some(9_980_000_000_000_000_020);
        assert_eq!(x1_amount_out(&reported, &two, TOKEN_OUT, SIGNER), Ok(()));
        assert!(x1_amount_out(&realised(), &two, TOKEN_OUT, SIGNER).is_err());
    }

    #[test]
    fn x2_the_amount_in_is_what_left_the_payer_less_what_came_back() {
        assert_eq!(
            x2_amount_in(&realised(), &receipt(), TOKEN_IN, SIGNER),
            Ok(())
        );
        let mut gross = realised();
        gross.amount_in = Some(10_000_000);
        let why = x2_amount_in(&gross, &receipt(), TOKEN_IN, SIGNER).unwrap_err();
        assert!(why.contains("10000000 out, 500 back"), "{why}");
        let mut none = realised();
        none.amount_in = None;
        assert!(x2_amount_in(&none, &receipt(), TOKEN_IN, SIGNER).is_err());
        // More came back than left: an error, not a wrapped number.
        let mut over = receipt();
        over["logs"]
            .as_array_mut()
            .unwrap()
            .push(log(TOKEN_IN, POOL, SIGNER, 99_000_000, 9));
        assert!(x2_amount_in(&realised(), &over, TOKEN_IN, SIGNER)
            .unwrap_err()
            .contains("came back"));
    }

    // --- X3, X4 ---

    #[test]
    fn x3_each_figure_of_the_cost_must_be_the_receipts() {
        assert_eq!(x3_cost(&realised(), &receipt()), Ok(()));
        for mutate in [
            (|r: &mut Realised| {
                if let TxCost::Evm(c) = &mut r.cost {
                    c.gas_used = Some(1)
                }
            }) as fn(&mut Realised),
            |r| {
                if let TxCost::Evm(c) = &mut r.cost {
                    c.effective_gas_price_wei = Some(1)
                }
            },
            |r| {
                if let TxCost::Evm(c) = &mut r.cost {
                    c.l1_fee_wei = Some(1)
                }
            },
            |r| {
                if let TxCost::Evm(c) = &mut r.cost {
                    c.l1_fee_wei = None
                }
            },
        ] {
            let mut changed = realised();
            mutate(&mut changed);
            assert!(x3_cost(&changed, &receipt()).is_err());
        }
        // A chain with no L1 fee: absent on both sides is a match.
        let mut no_l1 = receipt();
        no_l1.as_object_mut().unwrap().remove("l1Fee");
        let mut reported = realised();
        if let TxCost::Evm(c) = &mut reported.cost {
            c.l1_fee_wei = None;
        }
        assert_eq!(x3_cost(&reported, &no_l1), Ok(()));
    }

    #[test]
    fn the_fee_is_gas_times_price_plus_the_l1_fee_and_never_a_guess() {
        assert_eq!(fee_of(&receipt()), Ok(fee()));
        let mut no_l1 = receipt();
        no_l1.as_object_mut().unwrap().remove("l1Fee");
        assert_eq!(fee_of(&no_l1), Ok(0x249f0u128 * 0x5b8d80));
        for missing in ["gasUsed", "effectiveGasPrice"] {
            let mut broken = receipt();
            broken.as_object_mut().unwrap().remove(missing);
            assert!(fee_of(&broken).unwrap_err().contains(missing));
        }
    }

    #[test]
    fn x4_the_native_balance_fell_by_exactly_the_fee() {
        let before = 5_000_000_000_000_000u128;
        assert_eq!(x4_native_change(&receipt(), before, before - fee()), Ok(()));
        // One wei out either way, and a rise.
        assert!(x4_native_change(&receipt(), before, before - fee() + 1).is_err());
        assert!(x4_native_change(&receipt(), before, before - fee() - 1).is_err());
        let why = x4_native_change(&receipt(), before, before + 1).unwrap_err();
        assert!(why.contains("rose"), "{why}");
        // The L1 fee is part of it: leaving it out of the balance fails.
        let execution_only = 0x249f0u128 * 0x5b8d80;
        assert!(x4_native_change(&receipt(), before, before - execution_only).is_err());
    }

    // --- X5, X6 ---

    #[test]
    fn x5_each_token_moves_by_its_amount_and_a_revert_moves_neither() {
        let (inn, out) = (
            (U256::from(100_000_000u64), U256::from(90_000_500u64)),
            (U256::from(7), U256::from(7 + 9_980_000_000_000_000_000u128)),
        );
        assert_eq!(
            x5_token_changes(9_999_500, 9_980_000_000_000_000_000, inn, out),
            Ok(())
        );
        assert!(x5_token_changes(9_999_501, 9_980_000_000_000_000_000, inn, out).is_err());
        assert!(x5_token_changes(9_999_500, 9_980_000_000_000_000_001, inn, out).is_err());
        // A balance that went the wrong way is an error, not a wrapped number.
        let wrong_way = ((U256::from(1), U256::from(2)), out);
        assert!(x5_token_changes(0, 9_980_000_000_000_000_000, wrong_way.0, wrong_way.1).is_err());
        let unchanged = (U256::from(5), U256::from(5));
        assert_eq!(x5_token_changes(0, 0, unchanged, unchanged), Ok(()));
    }

    #[test]
    fn x6_the_landing_is_the_receipts_and_the_nonce_advanced_by_one() {
        assert_eq!(x6_landing(&realised(), &receipt(), 4, 5), Ok(()));
        type Mutation = (&'static str, fn(&mut Realised));
        let mutations: [Mutation; 5] = [
            ("Realised.at", |r| r.at = 1001),
            ("tx_ref", |r| r.tx_ref = Some(vec![0x12; 32])),
            ("tx_ref", |r| r.tx_ref = None),
            ("outcome", |r| {
                r.outcome = Outcome::Reverted { reason: "x".into() }
            }),
            ("provenance", |r| r.provenance = Provenance::Simulated),
        ];
        for (name, mutate) in mutations {
            let mut changed = realised();
            mutate(&mut changed);
            let why = x6_landing(&changed, &receipt(), 4, 5).unwrap_err();
            assert!(why.contains(name), "{name}: {why}");
        }
        for after in [4, 6] {
            assert!(x6_landing(&realised(), &receipt(), 4, after)
                .unwrap_err()
                .contains("nonce"));
        }
        // A revert agrees with a status of 0.
        let mut reverted = receipt();
        reverted["status"] = json!("0x0");
        let mut outcome = realised();
        outcome.outcome = Outcome::Reverted { reason: "x".into() };
        assert_eq!(x6_landing(&outcome, &reverted, 4, 5), Ok(()));
        assert!(x6_landing(&realised(), &reverted, 4, 5).is_err());
        // The hash compares without regard to case.
        let mut upper = receipt();
        upper["transactionHash"] = json!(HASH.to_ascii_uppercase().replace("0X", "0x"));
        assert_eq!(x6_landing(&realised(), &upper, 4, 5), Ok(()));
    }

    // --- the references ---

    fn quote() -> Quote {
        Quote {
            amount_out: U256::from(10_000_000_000_000_000_000u128),
            block: 998,
            local_ns: 1_700_000_000_000_000_000,
        }
    }

    fn facts<'a>(
        quote: Option<&'a Quote>,
        realised: Option<&'a Realised>,
        receipt: Option<&'a Value>,
    ) -> SwapFacts<'a> {
        SwapFacts {
            quote,
            calldata: &[0xab, 0xcd],
            router: Address::new([0x12; 20]),
            token_in: TOKEN_IN,
            token_out: TOKEN_OUT,
            amount_in: U256::from(10_000_000u64),
            sent_ns: Some(1_700_000_000_500_000_000),
            tx_hash: Some(B256::from([0x11; 32])),
            first_seen_ns: Some(1_700_000_001_000_000_000),
            sealed_ns: Some(1_700_000_002_000_000_000),
            realised,
            receipt,
            block_tx_count: Some(120),
        }
    }

    #[test]
    fn the_references_of_a_landed_swap_name_every_field_of_the_table() {
        let (quote, realised, receipt) = (quote(), realised(), receipt());
        let references = swap_references(&facts(Some(&quote), Some(&realised), Some(&receipt)));
        for field in SWAP_REFERENCE_FIELDS {
            assert!(references.get(field).is_some(), "{field} is absent");
        }
        assert_eq!(
            references["quote_pre_send"]["amount_out"],
            "10000000000000000000"
        );
        assert_eq!(references["quote_pre_send"]["block"], 998);
        assert_eq!(references["calldata"], "0xabcd");
        assert_eq!(references["amount_in"], "10000000");
        assert_eq!(references["inclusion_block"], 1000);
        assert_eq!(references["transaction_index"], 7);
        assert_eq!(references["block_tx_count"], 120);
        assert_eq!(references["transfer_log_indices"], json!([3, 4, 5, 6, 7]));
        assert_eq!(references["amount_in_taken"], "9999500");
        assert_eq!(references["amount_out"], "9980000000000000000");
        assert_eq!(references["gas_used"], 0x249f0);
        assert_eq!(references["l1_fee_wei"], (0x1d1a94a2000u64).to_string());
        assert_eq!(references["tx_hash"], B256::from([0x11; 32]).to_string());
        assert!(references["revert"].is_null());
        assert!(
            references["decision_simulation"].is_null(),
            "term 1 has no decision to simulate"
        );
        let (sent, seen, sealed) = (
            references["sent_ns"].as_u64().unwrap(),
            references["first_seen_ns"].as_u64().unwrap(),
            references["sealed_ns"].as_u64().unwrap(),
        );
        assert!(sent < seen && seen < sealed);
    }

    /// 9.98 against a quote of 10: 20 basis points under it.
    #[test]
    fn the_output_against_the_quote_is_in_basis_points_signed() {
        let (quote, realised, receipt) = (quote(), realised(), receipt());
        let references = swap_references(&facts(Some(&quote), Some(&realised), Some(&receipt)));
        assert_eq!(references["amount_out_vs_quote_bps"], "-20.0000");

        let mut better = Realised {
            amount_out: Some(10_010_000_000_000_000_000),
            ..realised.clone()
        };
        let references = swap_references(&facts(Some(&quote), Some(&better), Some(&receipt)));
        assert_eq!(references["amount_out_vs_quote_bps"], "10.0000");
        better.amount_out = None;
        let references = swap_references(&facts(Some(&quote), Some(&better), Some(&receipt)));
        assert!(references["amount_out_vs_quote_bps"].is_null());
        // A quote of nothing has no basis points.
        let zero = Quote {
            amount_out: U256::ZERO,
            ..quote.clone()
        };
        let references = swap_references(&facts(Some(&zero), Some(&realised), Some(&receipt)));
        assert!(references["amount_out_vs_quote_bps"].is_null());
    }

    /// A reference that does not exist is `null`, never zero or absent.
    #[test]
    fn a_reference_that_does_not_exist_is_null_never_zero_or_absent() {
        let mut nothing = facts(None, None, None);
        nothing.sent_ns = None;
        nothing.tx_hash = None;
        nothing.first_seen_ns = None;
        nothing.sealed_ns = None;
        nothing.block_tx_count = None;
        let references = swap_references(&nothing);
        for field in SWAP_REFERENCE_FIELDS {
            assert!(references.get(field).is_some(), "{field} is absent");
        }
        for field in [
            "quote_pre_send",
            "sent_ns",
            "tx_hash",
            "first_seen_ns",
            "sealed_ns",
            "inclusion_block",
            "transaction_index",
            "block_tx_count",
            "transfer_log_indices",
            "amount_in_taken",
            "amount_out",
            "gas_used",
            "effective_gas_price_wei",
            "l1_fee_wei",
            "amount_out_vs_quote_bps",
            "revert",
            "decision_simulation",
        ] {
            assert!(
                references[field].is_null(),
                "{field}: {}",
                references[field]
            );
        }
        // What the swap was is known without a landing.
        assert_eq!(references["amount_in"], "10000000");
        assert_eq!(references["calldata"], "0xabcd");
    }

    #[test]
    fn a_reverted_swap_has_its_reason_and_no_amounts() {
        let receipt = {
            let mut r = receipt();
            r["status"] = json!("0x0");
            r["logs"] = json!([]);
            r
        };
        let reverted = Realised {
            amount_out: None,
            amount_in: None,
            outcome: Outcome::Reverted {
                reason: "Router: INSUFFICIENT_OUTPUT_AMOUNT".into(),
            },
            ..realised()
        };
        let quote = quote();
        let references = swap_references(&facts(Some(&quote), Some(&reverted), Some(&receipt)));
        assert_eq!(references["revert"], "Router: INSUFFICIENT_OUTPUT_AMOUNT");
        for field in ["amount_in_taken", "amount_out", "amount_out_vs_quote_bps"] {
            assert!(references[field].is_null(), "{field}");
        }
        assert_eq!(references["transfer_log_indices"], json!([]));
        assert_eq!(references["gas_used"], 0x249f0, "a revert pays for its gas");
    }
}
