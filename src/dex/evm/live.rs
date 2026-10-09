//! `EvmLive` — signs and broadcasts a real EIP-1559 transaction, then polls
//! for the receipt (`SPEC.md` §5, first required implementation;
//! `IMPLEMENTATION_PLAN.md` Phases 5 and 10). Never exercised by an
//! automated test or a schedule — only ever a deliberate, human-triggered
//! run against a disposable, faucet-funded testnet signer (`SPEC.md`
//! §3/§9.4).
//!
//! It sends through a shared [`EvmSender`] and owns no key itself: every
//! adapter that sends from one wallet holds the same `Arc<EvmSender>`, which
//! is what keeps **one nonce in flight per chain at a time** true for the
//! wallet rather than for each adapter. There is deliberately no
//! constructor that builds a sender from a key: it would make a second
//! sender for the same wallet the easy thing to write.
//!
//! **Adapter convention for `RouteQuote.payload`:** same as `EvmSimulated`
//! — `router_address (20 bytes) ++ calldata`. This adapter never inspects
//! or rebuilds that calldata; it only signs and sends it, so whatever
//! encoded it is responsible for its correctness (including, e.g., any
//! `amountOutMinimum` the router itself checks).
//!
//! **`SwapRequest.deadline_unix_secs` enforcement:** `SPEC.md` §5 asks for
//! a real deadline "never omitted". Because this adapter's calldata is
//! opaque and already fully built by the caller (see above), it cannot
//! embed a deadline into a call that doesn't already carry one (Uniswap's
//! `SwapRouter02#exactInputSingle`, used by the real-network tests
//! elsewhere in this crate, has no deadline parameter at all). What this
//! adapter *can* and does enforce: `prepare()` refuses a zero/omitted
//! deadline outright, and `execute()` refuses to broadcast anything once
//! wall-clock time has passed it — a weaker guarantee than an on-chain
//! deadline check, but the strongest available without changing what
//! calldata means.
//!
//! **The venue cap, never an unlimited allowance** (`SPEC.md` §5): before
//! ever sending the swap, [`EvmSender::ensure_allowance`] checks the
//! router's current allowance and, only if it's short, sends an `approve`
//! capped to exactly `route.amount_in` — never `U256::MAX`.
//!
//! **A contract payer** (`Payer::CalledContract`): the contract called holds
//! the input and pays the venue itself, so nothing is approved and no
//! allowance is read. The amount out is read the same way, from the output's
//! `Transfer` to `SwapRequest.recipient`.
//!
//! **The amount in** is what the pool took, which a swap that reaches its
//! price limit makes less than the route's `amount_in`: the input token's
//! `Transfer`s out of the payer (the sender, or the contract called), less
//! any `Transfer` back to it (a router refunding what it did not use). A
//! landed swap with none is an error naming its transaction, as one with no
//! output `Transfer` is.
//!
//! **A swap that timed out can be resolved later** ([`EvmLive::resolve`]).
//! When a send ends `TimedOut`, the swap's context is kept by the
//! transaction's hash, and `resolve(hash)` asks the sender about it once: still
//! pending, mined (read exactly as `execute` reads a landed swap, through the
//! same code), or replaced or dropped. A failed read is an error and keeps
//! everything, so the same call can be made again. What is kept is in memory
//! only: after a restart there is nothing to resolve through.
//!
//! **Over a fork sender** (`SPEC.md` §5b), this adapter is a swap simulator
//! whose state persists between calls: its provenance is `Simulated`, and its
//! `tx_ref` is the fork transaction's hash, as a live send's is. The payer
//! must hold the input token on the fork; [`EvmSender::ensure_balance`]
//! writes it there.

use crate::dex::{
    ChainAmount, DexExecutor, EvmCall, EvmCost, Outcome, Payer, Prepared, Realised, Resolution,
    RouteQuote, SwapRequest, TxCost,
};
use crate::evm::erc20;
use crate::evm::rpc::address_from_slice;
use crate::evm::{prepared_key, EvmSender, RpcLog, TxOutcome, TxReplacedOrDropped};
use crate::Network;
use crate::Provenance;
use alloy_primitives::{Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy)]
struct PendingLive {
    token_in: Address,
    token_out: Address,
    sender: Address,
    recipient: Address,
    payer: Payer,
    amount_in: ChainAmount,
    deadline_unix_secs: u64,
}

/// A swap whose send ended `TimedOut`: what is needed to read its result from
/// its receipt once the chain has one.
#[derive(Debug, Clone, Copy)]
struct TimedOutSwap {
    swap: PendingLive,
    router: Address,
}

pub struct EvmLive {
    sender: Arc<EvmSender>,
    /// Prepared and not yet sent, by `prepared_key`.
    pending: Mutex<HashMap<B256, PendingLive>>,
    /// Sent and ended `TimedOut`, by transaction hash, until [`EvmLive::resolve`]
    /// has found how it ended.
    timed_out: Mutex<HashMap<B256, TimedOutSwap>>,
}

impl EvmLive {
    pub fn new(sender: Arc<EvmSender>) -> Self {
        Self {
            sender,
            pending: Mutex::new(HashMap::new()),
            timed_out: Mutex::new(HashMap::new()),
        }
    }

    /// The sender's address — the only address this adapter will ever
    /// accept as a `SwapRequest.sender`.
    pub fn address(&self) -> Address {
        self.sender.address()
    }

    pub fn sender(&self) -> &Arc<EvmSender> {
        &self.sender
    }

    /// What became of a swap this adapter's `execute` ended `TimedOut`, asked
    /// once: [`Resolution::Pending`] while the chain has not decided,
    /// [`Resolution::Done`] with the swap's [`Realised`] once it is mined (a
    /// revert included), [`Resolution::Gone`] once it was replaced or dropped.
    /// `tx_hash` is the hash that `Realised.tx_ref` carried.
    ///
    /// A swap that is `Done` or `Gone` is forgotten, and the sender's latch is
    /// clear, so it can send again. An `Err` is a read that failed, or a hash
    /// this adapter did not time out, or one its sender no longer holds
    /// unresolved (something resolved the sender directly, which hands the
    /// outcome to that caller and not to this adapter: resolve through here).
    /// After an `Err` from a failed read, everything is as it was, and the same
    /// call can be made again. A failed read is never `Gone`. A swap that landed
    /// with logs its decoders cannot read is an `Err` too, as from `execute`,
    /// but one answer only: it is decided, so it is forgotten, and the same call
    /// again says there is nothing to resolve.
    pub async fn resolve(&self, tx_hash: B256) -> Result<Resolution> {
        let TimedOutSwap { swap, router } = self
            .timed_out
            .lock()
            .unwrap()
            .get(&tx_hash)
            .copied()
            .ok_or_else(|| {
            anyhow!(
                "this EvmLive did not time out a swap with hash {tx_hash}, or has already \
                     resolved it: nothing to resolve"
            )
        })?;
        // Not the sender's own `resolve` for a hash it does not hold: that would
        // clear and consume another adapter's unresolved transaction.
        if self.sender.unresolved() != Some(tx_hash) {
            bail!(
                "swap {tx_hash} timed out, but its sender no longer holds it as unresolved: it \
                 was resolved through the sender directly, which hands the outcome to that caller"
            );
        }
        match self.sender.resolve().await {
            Ok(Some(outcome)) => {
                let (TxOutcome::Success { tx_hash: found, .. }
                | TxOutcome::Reverted { tx_hash: found, .. }
                | TxOutcome::TimedOut { tx_hash: found }) = &outcome;
                if *found != tx_hash {
                    bail!(
                        "resolving swap {tx_hash}, the sender resolved {found} instead: it held \
                         another transaction"
                    );
                }
                // The sender has handed the outcome over and cleared its latch:
                // the swap is decided, whether or not its logs can be read, so
                // it is forgotten before they are.
                self.timed_out.lock().unwrap().remove(&tx_hash);
                Ok(Resolution::Done(
                    self.realised(&swap, router, outcome).await?,
                ))
            }
            Ok(None) if self.sender.unresolved() == Some(tx_hash) => Ok(Resolution::Pending),
            Ok(None) => bail!(
                "swap {tx_hash} timed out, but its sender no longer holds it as unresolved: it \
                 was resolved through the sender directly, which hands the outcome to that caller"
            ),
            Err(err) => match err.downcast_ref::<TxReplacedOrDropped>() {
                Some(dropped) if dropped.tx_hash == tx_hash => {
                    self.timed_out.lock().unwrap().remove(&tx_hash);
                    Ok(Resolution::Gone)
                }
                _ => Err(err.context(format!("resolving swap {tx_hash}"))),
            },
        }
    }

    /// A swap's [`Realised`] from the outcome of its transaction. One reading,
    /// whether `execute` waited for the transaction or `resolve` found it
    /// later, so the amounts they report cannot disagree: what the pool took is
    /// what is booked.
    async fn realised(
        &self,
        ctx: &PendingLive,
        router: Address,
        outcome: TxOutcome,
    ) -> Result<Realised> {
        match outcome {
            TxOutcome::Success {
                block,
                tx_hash,
                logs,
                cost,
            } => {
                let amount_out = decode_transfer_amount(&logs, ctx.token_out, ctx.recipient)
                    .ok_or_else(|| {
                        anyhow!(
                            "swap landed (tx {tx_hash}) but no ERC-20 Transfer of the output \
                             token to the recipient was found in its logs"
                        )
                    })?;
                let payer = match ctx.payer {
                    Payer::Sender => ctx.sender,
                    Payer::CalledContract => router,
                };
                let amount_in =
                    decode_amount_spent(&logs, ctx.token_in, payer).ok_or_else(|| {
                        anyhow!(
                            "swap landed (tx {tx_hash}) but no ERC-20 Transfer of the input token \
                         out of its payer {payer} was found in its logs"
                        )
                    })?;
                Ok(Realised {
                    amount_out: Some(amount_out),
                    amount_in: Some(amount_in),
                    outcome: Outcome::Success,
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
            } => Ok(Realised {
                amount_out: None,
                amount_in: None,
                outcome: Outcome::Reverted { reason },
                cost: TxCost::Evm(cost),
                at: block,
                provenance: self.sender.provenance(),
                tx_ref: Some(tx_hash.as_slice().to_vec()),
            }),
            TxOutcome::TimedOut { tx_hash } => {
                let at = self.sender.rpc().block_number().await.unwrap_or(0);
                Ok(Realised {
                    amount_out: None,
                    amount_in: None,
                    outcome: Outcome::TimedOut,
                    cost: TxCost::Evm(EvmCost::default()),
                    at,
                    provenance: self.sender.provenance(),
                    tx_ref: Some(tx_hash.as_slice().to_vec()),
                })
            }
        }
    }
}

#[async_trait]
impl DexExecutor for EvmLive {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        if route.payload.len() < 20 {
            bail!(
                "route.payload must be at least 20 bytes (router address + calldata), got {}",
                route.payload.len()
            );
        }
        req.priority.policy_only("EvmLive")?;
        if route.network != Network::evm(self.sender.chain_id()) {
            bail!(
                "route is for network {}, but this EvmLive's sender is on chain {}",
                route.network,
                self.sender.chain_id()
            );
        }
        let (router_bytes, calldata) = route.payload.split_at(20);
        let token_in = address_from_slice(&route.token_in).context("route.token_in")?;
        let token_out = address_from_slice(&route.token_out).context("route.token_out")?;
        let sender = address_from_slice(&req.sender).context("req.sender")?;
        let recipient = address_from_slice(&req.recipient).context("req.recipient")?;

        if sender.is_zero() {
            bail!("SwapRequest.sender must not be the zero address for EvmLive");
        }
        if recipient.is_zero() {
            bail!("SwapRequest.recipient must not be the zero address for EvmLive");
        }
        if req.deadline_unix_secs == 0 {
            bail!(
                "SwapRequest.deadline_unix_secs must be set for EvmLive — see the module docs \
                 for how it's enforced given this adapter's opaque-calldata convention"
            );
        }
        if sender != self.sender.address() {
            bail!(
                "SwapRequest.sender ({sender}) does not match this EvmLive's sending address \
                 ({}) — it can only ever send as its own sender",
                self.sender.address()
            );
        }

        let call = EvmCall {
            to: router_bytes.to_vec(),
            calldata: calldata.to_vec(),
            value: 0,
        };
        self.pending.lock().unwrap().insert(
            prepared_key(&call),
            PendingLive {
                token_in,
                token_out,
                sender,
                recipient,
                payer: req.payer,
                amount_in: route.amount_in,
                deadline_unix_secs: req.deadline_unix_secs,
            },
        );
        Ok(Prepared::Evm(call))
    }

    /// `at` is ignored: a live adapter only ever acts *now* — there is no
    /// "re-run this at a historical block" for a real broadcast the way
    /// there is for `EvmSimulated`'s throwaway `eth_call`.
    async fn execute(&self, prepared: &Prepared, _at: Option<u64>) -> Result<Realised> {
        let prepared = prepared.evm_call()?;
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&prepared_key(prepared))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this EvmLive instance did not \
                     produce, or already consumed"
                )
            })?;
        let router = address_from_slice(&prepared.to).context("prepared.to")?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now >= ctx.deadline_unix_secs {
            bail!(
                "SwapRequest.deadline_unix_secs ({}) has already passed (now {now}) — \
                 refusing to broadcast",
                ctx.deadline_unix_secs
            );
        }

        match ctx.payer {
            Payer::Sender => {
                self.sender
                    .ensure_allowance(ctx.token_in, router, U256::from(ctx.amount_in))
                    .await?
            }
            // The contract pays the venue from its own balance: nothing to approve.
            Payer::CalledContract => {}
        }

        let outcome = self
            .sender
            .send_and_confirm(router, prepared.calldata.clone(), U256::ZERO)
            .await?;
        if let TxOutcome::TimedOut { tx_hash } = &outcome {
            // The sender keeps the hash unresolved; keep what is needed to read
            // the swap's result by it, for `resolve`.
            self.timed_out
                .lock()
                .unwrap()
                .insert(*tx_hash, TimedOutSwap { swap: ctx, router });
        }
        self.realised(&ctx, router, outcome).await
    }

    fn label(&self) -> &'static str {
        match self.sender.provenance() {
            Provenance::Landed => "evm-live",
            Provenance::Simulated => "evm-live-fork",
        }
    }
}

/// The value of the ERC-20 `Transfer` of `token` to `recipient` in a
/// receipt's logs. A router swap may emit several `Transfer`s (intermediate
/// hops, fee transfers); filtering by both the token and the recipient is
/// what picks out the one that's actually this swap's output. An amount
/// above `u128` is `None`, never truncated.
fn decode_transfer_amount(
    logs: &[RpcLog],
    token: Address,
    recipient: Address,
) -> Option<ChainAmount> {
    erc20::transfers(logs)
        .into_iter()
        .find(|t| t.token == token && t.to == recipient)
        .and_then(|t| t.value.try_into().ok())
}

/// What `payer` gave up of `token` in a receipt's logs: its `Transfer`s out,
/// less the `Transfer`s back to it. `None` when nothing net left it, and for
/// an amount above `u128`, never truncated.
fn decode_amount_spent(logs: &[RpcLog], token: Address, payer: Address) -> Option<ChainAmount> {
    let (mut out, mut back) = (U256::ZERO, U256::ZERO);
    for transfer in erc20::transfers(logs)
        .into_iter()
        .filter(|t| t.token == token && t.from != t.to)
    {
        if transfer.from == payer {
            out = out.saturating_add(transfer.value);
        }
        if transfer.to == payer {
            back = back.saturating_add(transfer.value);
        }
    }
    out.checked_sub(back)
        .filter(|spent| !spent.is_zero())
        .and_then(|spent| spent.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::PriorityBid;
    use crate::evm::erc20::{transfer_topic, ALLOWANCE_SELECTOR};
    use crate::evm::rpc::{encode_error_string, format_u256, hex_data, pad_address};
    use crate::evm::tx::tests::{connect, mount_receipt, mount_send_plumbing, SEPOLIA};
    use crate::evm::{EvmRpc, FeePolicy, PollSettings, Signer};
    use serde_json::{json, Value};
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// An ERC-20 `Transfer` log.
    fn transfer(token: Address, from: Address, to: Address, amount: u64) -> Value {
        json!({
            "address": token.to_string(),
            "topics": [
                transfer_topic().to_string(),
                hex_data(&pad_address(from)),
                hex_data(&pad_address(to)),
            ],
            "data": format_u256(U256::from(amount)),
        })
    }

    fn route(
        router: Address,
        token_in: Address,
        token_out: Address,
        calldata: &[u8],
    ) -> RouteQuote {
        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(calldata);
        RouteQuote {
            network: Network::evm(SEPOLIA),
            token_in: token_in.as_slice().to_vec(),
            token_out: token_out.as_slice().to_vec(),
            amount_in: 1_000,
            expected_amount_out: 900,
            payload,
        }
    }

    fn request(sender: Address, recipient: Address) -> SwapRequest {
        SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: recipient.as_slice().to_vec(),
            payer: Payer::Sender,
            min_amount_out: 900,
            deadline_unix_secs: 9_999_999_999,
            priority: PriorityBid::Policy,
        }
    }

    /// An `EvmLive` over a sender connected to a mock node that only knows
    /// its chain id — enough for `prepare`, which never touches the network.
    async fn offline_live() -> (MockServer, EvmLive) {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let live = EvmLive::new(connect(&server).await);
        (server, live)
    }

    #[tokio::test]
    async fn prepare_rejects_the_zero_address_as_sender_or_recipient() {
        let (_server, live) = offline_live().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);

        let zero_sender = SwapRequest {
            sender: Address::ZERO.as_slice().to_vec(),
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        assert!(live.prepare(&route, &zero_sender).await.is_err());

        let zero_recipient = SwapRequest {
            recipient: Address::ZERO.as_slice().to_vec(),
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        assert!(live.prepare(&route, &zero_recipient).await.is_err());
    }

    #[tokio::test]
    async fn prepare_rejects_an_omitted_deadline() {
        let (_server, live) = offline_live().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = SwapRequest {
            deadline_unix_secs: 0,
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        let err = live.prepare(&route, &req).await.unwrap_err();
        assert!(err.to_string().contains("deadline_unix_secs"));
    }

    #[tokio::test]
    async fn prepare_rejects_a_sender_that_is_not_this_senders_own_address() {
        let (_server, live) = offline_live().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(Address::from([0x99; 20]), Address::from([0xDD; 20]));
        let err = live.prepare(&route, &req).await.unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn prepare_rejects_a_route_for_another_chain() {
        let (_server, live) = offline_live().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = RouteQuote {
            network: Network::evm(1),
            ..route(router, token, token, &[0xCA, 0xFE])
        };
        let req = request(live.address(), Address::from([0xDD; 20]));
        let err = live.prepare(&route, &req).await.unwrap_err();
        assert!(err.to_string().contains("chain 1"));
    }

    /// Mounts an `eth_call` handler that reports a sufficient allowance
    /// already in place, so `ensure_allowance` never sends an `approve` —
    /// isolating a test to exactly one `send_and_confirm` (the swap).
    /// Scoped to calls whose data carries the `allowance` selector: two
    /// equal-priority mocks that both matched the same request would
    /// resolve to whichever was registered first (`wiremock`'s
    /// `mock_set.rs` sorts by priority, stable, then takes the first
    /// match), so leaving this unscoped would silently swallow the
    /// revert-reason replay call in `execute_reports_the_solidity_revert_reason`
    /// too if it happened to be mounted first there.
    async fn mount_sufficient_allowance(server: &MockServer) {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .and(body_string_contains(hex::encode(ALLOWANCE_SELECTOR)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": format_u256(U256::MAX),
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn execute_approves_then_swaps_and_decodes_the_output_transfer() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token_in = Address::from([0xAA; 20]);
        let token_out = Address::from([0xBB; 20]);
        let recipient = Address::from([0xDD; 20]);
        let calldata = vec![0xCA, 0xFE, 0xBA, 0xBE];

        mount_send_plumbing(&server).await;

        // Allowance check reports zero (forcing an `approve`) for any
        // `eth_call` whose data starts with the `allowance` selector;
        // anything else (there's no revert-reason replay on this happy
        // path) gets an empty-but-valid response.
        let allowance_selector_hex = hex::encode(ALLOWANCE_SELECTOR);
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let data = body["params"][0]["data"].as_str().unwrap_or("");
                let result = if data.starts_with(&format!("0x{allowance_selector_hex}")) {
                    format_u256(U256::ZERO)
                } else {
                    "0x".to_string()
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(&server)
            .await;

        let live = EvmLive::new(connect(&server).await);
        let sender = live.address();
        mount_receipt(
            &server,
            json!({
                "status": "0x1", "blockNumber": "0x2a",
                "logs": [
                    transfer(token_in, sender, router, 1_000),
                    transfer(token_out, router, recipient, 4_200),
                ],
                "gasUsed": "0x249f0", "effectiveGasPrice": "0x77359400", "l1Fee": "0x7",
            }),
        )
        .await;

        let route = route(router, token_in, token_out, &calldata);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, Some(4_200));
        assert_eq!(realised.amount_in, Some(1_000));
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(
            realised.cost,
            TxCost::Evm(EvmCost {
                gas_used: Some(150_000),
                effective_gas_price_wei: Some(2_000_000_000),
                l1_fee_wei: Some(7),
            })
        );
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        assert_eq!(realised.at, 42);
    }

    /// A contract that keeps its own inventory pays the pool itself: no
    /// allowance is read and no `approve` is sent, so the swap is the only
    /// transaction. Its output is the `Transfer` to the contract.
    #[tokio::test]
    async fn a_contract_payer_sends_the_swap_and_nothing_else() {
        let server = MockServer::start().await;
        let contract = Address::from([0x11; 20]);
        let token_in = Address::from([0xAA; 20]);
        let token_out = Address::from([0xBB; 20]);
        let pool = Address::from([0x22; 20]);

        mount_send_plumbing(&server).await;
        // An allowance read here would find none, and send an approve.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::ZERO),
            })))
            .mount(&server)
            .await;
        mount_receipt(
            &server,
            json!({
                "status": "0x1", "blockNumber": "0x2a",
                "logs": [
                    transfer(token_in, contract, pool, 1_000),
                    transfer(token_out, pool, contract, 4_200),
                ],
            }),
        )
        .await;

        let live = EvmLive::new(connect(&server).await);
        let req = SwapRequest {
            payer: Payer::CalledContract,
            ..request(live.address(), contract)
        };
        let prepared = live
            .prepare(&route(contract, token_in, token_out, &[0xCA, 0xFE]), &req)
            .await
            .unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();
        assert_eq!(realised.amount_out, Some(4_200));
        assert_eq!(
            realised.amount_in,
            Some(1_000),
            "what the contract paid the pool"
        );
        assert!(matches!(realised.outcome, Outcome::Success));

        let bodies: Vec<Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.body_json().unwrap())
            .collect();
        let count = |rpc_method: &str| bodies.iter().filter(|b| b["method"] == rpc_method).count();
        assert_eq!(
            count("eth_sendRawTransaction"),
            1,
            "the swap alone, no approve"
        );
        assert_eq!(count("eth_call"), 0, "no allowance read");
    }

    /// Over a fork sender the swap is `Simulated`, and carries the fork
    /// transaction's hash all the same: a transaction was sent (`SPEC.md`
    /// §5's shape rule, amended 3 October 2026).
    #[tokio::test]
    async fn over_a_fork_sender_the_swap_is_simulated_and_carries_its_hash() {
        let server = MockServer::start().await;
        let owner = Address::from([0xCC; 20]);
        let contract = Address::from([0x11; 20]);
        let token_in = Address::from([0xAA; 20]);
        let token_out = Address::from([0xBB; 20]);
        let tx_hash = B256::repeat_byte(0xAB);
        for (rpc_method, result) in [
            ("web3_clientVersion", json!("anvil/v1.3.0")),
            ("eth_chainId", json!(format!("0x{SEPOLIA:x}"))),
            (
                "eth_getBalance",
                json!(format_u256(U256::from(10u128.pow(20)))),
            ),
            ("anvil_impersonateAccount", Value::Null),
            ("eth_getTransactionCount", json!("0x5")),
            ("eth_estimateGas", json!("0x5208")),
            ("eth_sendTransaction", json!(tx_hash.to_string())),
            ("eth_blockNumber", json!("0x2a")),
        ] {
            Mock::given(method("POST"))
                .and(body_partial_json(json!({ "method": rpc_method })))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result })),
                )
                .mount(&server)
                .await;
        }
        let pool = Address::from([0x22; 20]);
        mount_receipt(
            &server,
            json!({
                "status": "0x1", "blockNumber": "0x2a",
                "logs": [
                    transfer(token_in, contract, pool, 1_000),
                    transfer(token_out, pool, owner, 4_200),
                ],
            }),
        )
        .await;

        let sender = EvmSender::fork(EvmRpc::new(server.uri()), owner, SEPOLIA)
            .await
            .unwrap();
        let live = EvmLive::new(sender);
        assert_eq!(live.label(), "evm-live-fork");
        let fixture = crate::testkit::contract::DexContractFixture {
            route: route(contract, token_in, token_out, &[0xCA, 0xFE]),
            request: SwapRequest {
                payer: Payer::CalledContract,
                ..request(owner, owner)
            },
        };
        let prepared = live
            .prepare(&fixture.route, &fixture.request)
            .await
            .unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();
        assert_eq!(realised.amount_out, Some(4_200));
        assert_eq!(realised.amount_in, Some(1_000));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, Some(tx_hash.as_slice().to_vec()));

        crate::testkit::contract::dex_executor_contract(
            &live,
            crate::testkit::contract::Sends::Transactions,
            fixture,
        )
        .await;
    }

    #[tokio::test]
    async fn satisfies_the_dex_contract() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token_out = Address::from([0xBB; 20]);
        let recipient = Address::from([0xDD; 20]);
        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        let live = EvmLive::new(connect(&server).await);
        mount_receipt(
            &server,
            json!({
                "status": "0x1", "blockNumber": "0x2a",
                "logs": [
                    transfer(Address::from([0xAA; 20]), live.address(), router, 1_000),
                    transfer(token_out, router, recipient, 950),
                ],
            }),
        )
        .await;

        let fixture = crate::testkit::contract::DexContractFixture {
            route: route(router, Address::from([0xAA; 20]), token_out, &[0xCA, 0xFE]),
            request: request(live.address(), recipient),
        };
        crate::testkit::contract::dex_executor_contract(
            &live,
            crate::testkit::contract::Sends::Transactions,
            fixture,
        )
        .await;
    }

    #[tokio::test]
    async fn execute_reports_the_solidity_revert_reason() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let recipient = Address::from([0xDD; 20]);

        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        mount_receipt(
            &server,
            json!({ "status": "0x0", "blockNumber": "0x2a", "logs": [] }),
        )
        .await;

        // The revert-reason replay: any `eth_call` that isn't the
        // allowance probe (mounted first above, so matched first) hits this.
        let reason = "insufficient liquidity";
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": 3, "message": "execution reverted",
                           "data": hex_data(&encode_error_string(reason)) },
            })))
            .mount(&server)
            .await;

        let live = EvmLive::new(connect(&server).await);
        let sender = live.address();
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.amount_in, None, "a revert took no input");
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        match realised.outcome {
            Outcome::Reverted { reason: got } => assert_eq!(got, reason),
            other => panic!("expected Reverted, got {other:?}"),
        }
    }

    /// One swap of 1 000 through a router, by `payer`, whose receipt carries
    /// the logs `logs` builds from the sender's address.
    async fn swap_landing_with(
        payer: Payer,
        logs: impl FnOnce(Address) -> Vec<Value>,
    ) -> Result<Realised> {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        let live = EvmLive::new(connect(&server).await);
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2a", "logs": logs(live.address()) }),
        )
        .await;
        let route = route(
            Address::from([0x11; 20]),
            Address::from([0xAA; 20]),
            Address::from([0xBB; 20]),
            &[0xCA, 0xFE],
        );
        let req = SwapRequest {
            payer,
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        let prepared = live.prepare(&route, &req).await?;
        live.execute(&prepared, None).await
    }

    /// A V3 swap that reaches its price limit takes less than it was given:
    /// what is booked is what left the payer, not what the route offered.
    #[tokio::test]
    async fn a_swap_that_took_less_than_its_offer_reports_what_the_pool_took() {
        let (router, pool, recipient) = (
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            Address::from([0xDD; 20]),
        );
        let (token_in, token_out) = (Address::from([0xAA; 20]), Address::from([0xBB; 20]));
        let realised = swap_landing_with(Payer::Sender, |sender| {
            vec![
                transfer(token_in, sender, pool, 600),
                transfer(token_out, pool, recipient, 550),
            ]
        })
        .await
        .unwrap();
        assert_eq!(realised.amount_in, Some(600), "the offer was 1 000");
        assert_eq!(realised.amount_out, Some(550));

        // A router that pulled the whole offer and refunded the rest: the
        // refund is netted off what it pulled.
        let realised = swap_landing_with(Payer::Sender, |sender| {
            vec![
                transfer(token_in, sender, router, 1_000),
                transfer(token_in, router, sender, 400),
                transfer(token_in, router, pool, 600),
                transfer(token_out, pool, recipient, 550),
            ]
        })
        .await
        .unwrap();
        assert_eq!(realised.amount_in, Some(600));
    }

    /// Only the payer's own input counts: another address's transfers of the
    /// same token, and the payer's transfers of another token, do not.
    #[tokio::test]
    async fn only_the_payers_own_transfers_of_the_input_token_count() {
        let (pool, recipient, other) = (
            Address::from([0x22; 20]),
            Address::from([0xDD; 20]),
            Address::from([0x33; 20]),
        );
        let (token_in, token_out) = (Address::from([0xAA; 20]), Address::from([0xBB; 20]));
        let realised = swap_landing_with(Payer::Sender, |sender| {
            vec![
                transfer(token_in, other, pool, 5_000),
                transfer(token_out, sender, other, 77),
                transfer(token_in, sender, pool, 1_000),
                transfer(token_out, pool, recipient, 950),
            ]
        })
        .await
        .unwrap();
        assert_eq!(realised.amount_in, Some(1_000));
    }

    /// With a contract payer the input leaves the contract called, and the
    /// sender's own transfers are not the swap's.
    #[tokio::test]
    async fn a_contract_payers_input_is_what_the_contract_called_paid() {
        let (router, pool, recipient) = (
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            Address::from([0xDD; 20]),
        );
        let (token_in, token_out) = (Address::from([0xAA; 20]), Address::from([0xBB; 20]));
        let logs = move |sender: Address| {
            vec![
                transfer(token_in, router, pool, 600),
                transfer(token_in, sender, pool, 9),
                transfer(token_out, pool, recipient, 550),
            ]
        };
        let realised = swap_landing_with(Payer::CalledContract, logs)
            .await
            .unwrap();
        assert_eq!(realised.amount_in, Some(600));

        // The same receipt for a swap the sender paid is the sender's 9.
        let realised = swap_landing_with(Payer::Sender, logs).await.unwrap();
        assert_eq!(realised.amount_in, Some(9));
    }

    /// A swap that landed with no `Transfer` of the input out of its payer is
    /// an error that says so, as one with no output `Transfer` is: a figure
    /// guessed from the offer would be booked as what the pool took.
    #[tokio::test]
    async fn a_landed_swap_with_no_input_transfer_is_an_error() {
        let (pool, recipient) = (Address::from([0x22; 20]), Address::from([0xDD; 20]));
        let (token_in, token_out) = (Address::from([0xAA; 20]), Address::from([0xBB; 20]));
        let err = swap_landing_with(Payer::Sender, |_| {
            vec![
                transfer(token_in, pool, recipient, 1_000),
                transfer(token_out, pool, recipient, 950),
            ]
        })
        .await
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("swap landed")
                && text.contains("no ERC-20 Transfer of the input token out of its payer"),
            "{text}"
        );

        // Nor does an input that came straight back count as spent.
        let err = swap_landing_with(Payer::Sender, |sender| {
            vec![
                transfer(token_in, sender, pool, 1_000),
                transfer(token_in, pool, sender, 1_000),
                transfer(token_out, pool, recipient, 950),
            ]
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("input token"), "{err}");
    }

    #[tokio::test]
    async fn execute_times_out_when_no_receipt_ever_arrives() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let recipient = Address::from([0xDD; 20]);

        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        mount_receipt(&server, Value::Null).await;

        let sender = connect(&server).await;
        sender.set_poll_settings(PollSettings {
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(20),
        });
        let live = EvmLive::new(sender);
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(live.address(), recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.amount_in, None, "a timeout says nothing was taken");
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        assert!(matches!(realised.outcome, Outcome::TimedOut));
        assert_eq!(
            live.sender().unresolved().map(|h| h.as_slice().to_vec()),
            realised.tx_ref
        );

        // The timed-out swap blocks the next one until it is resolved.
        let prepared = live.prepare(&route, &req).await.unwrap();
        let err = live.execute(&prepared, None).await.unwrap_err();
        assert!(err.to_string().contains("unresolved"));
    }

    const ROUTER: Address = Address::new([0x11; 20]);
    const POOL: Address = Address::new([0x22; 20]);
    const TOKEN_IN: Address = Address::new([0xAA; 20]);
    const TOKEN_OUT: Address = Address::new([0xBB; 20]);
    const RECIPIENT: Address = Address::new([0xDD; 20]);

    fn rpc_ok(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }

    /// An adapter whose one swap of 1 000 has timed out: the node knows the
    /// transaction and has no receipt for it. Returns the guard of the "no
    /// receipt" mock, which a test drops when the transaction is mined.
    async fn timed_out_swap() -> Result<(MockServer, EvmLive, B256, wiremock::MockGuard)> {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        let no_receipt = Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(rpc_ok(Value::Null))
            .mount_as_scoped(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionByHash"}),
            ))
            .respond_with(rpc_ok(json!({ "hash": "0x01" })))
            .mount(&server)
            .await;

        let sender = connect(&server).await;
        sender.set_poll_settings(PollSettings {
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(20),
        });
        let live = EvmLive::new(sender);
        let prepared = live
            .prepare(
                &route(ROUTER, TOKEN_IN, TOKEN_OUT, &[0xCA, 0xFE]),
                &request(live.address(), RECIPIENT),
            )
            .await?;
        let realised = live.execute(&prepared, None).await?;
        assert!(matches!(realised.outcome, Outcome::TimedOut));
        let tx_ref = realised
            .tx_ref
            .ok_or_else(|| anyhow!("a timeout names its hash"))?;
        let tx_hash = B256::try_from(tx_ref.as_slice())?;
        Ok((server, live, tx_hash, no_receipt))
    }

    /// The pool took 600 of the 1 000 offered, and paid 550.
    fn pool_took_600(sender: Address) -> Vec<Value> {
        vec![
            transfer(TOKEN_IN, sender, POOL, 600),
            transfer(TOKEN_OUT, POOL, RECIPIENT, 550),
        ]
    }

    /// While the node still knows the transaction the swap is pending, however
    /// often it is asked; once it is mined the result is what `execute` would
    /// have reported for the same receipt, what the pool took included; then
    /// the swap is forgotten and the sender can send again.
    #[tokio::test]
    async fn a_timed_out_swap_found_mined_is_done_with_what_the_pool_took() -> Result<()> {
        let (server, live, tx_hash, no_receipt) = timed_out_swap().await?;

        assert!(matches!(live.resolve(tx_hash).await?, Resolution::Pending));
        assert!(matches!(live.resolve(tx_hash).await?, Resolution::Pending));
        assert_eq!(live.sender().unresolved(), Some(tx_hash));

        drop(no_receipt);
        let receipt = json!({
            "status": "0x1", "blockNumber": "0x2b",
            "logs": pool_took_600(live.address()),
            "gasUsed": "0x249f0", "effectiveGasPrice": "0x77359400", "l1Fee": "0x7",
        });
        mount_receipt(&server, receipt).await;
        let Resolution::Done(realised) = live.resolve(tx_hash).await? else {
            bail!("a mined swap is Done");
        };

        assert_eq!(realised.amount_in, Some(600), "the offer was 1 000");
        assert_eq!(realised.amount_out, Some(550));
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.at, 0x2b);
        assert_eq!(realised.provenance, Provenance::Landed);
        assert_eq!(realised.tx_ref, Some(tx_hash.as_slice().to_vec()));

        // The same receipt, found by a send that waited for it.
        let sent = swap_landing_with(Payer::Sender, pool_took_600).await?;
        assert_eq!(
            (sent.amount_in, sent.amount_out),
            (realised.amount_in, realised.amount_out)
        );
        assert_eq!(sent.outcome, realised.outcome);
        assert_eq!(sent.provenance, realised.provenance);
        // What it cost is what its receipt says.
        assert_eq!(
            realised.cost,
            TxCost::Evm(EvmCost {
                gas_used: Some(150_000),
                effective_gas_price_wei: Some(2_000_000_000),
                l1_fee_wei: Some(7),
            })
        );

        // Forgotten, and the sender is free.
        assert_eq!(live.sender().unresolved(), None);
        let err = live.resolve(tx_hash).await.unwrap_err();
        assert!(err.to_string().contains("nothing to resolve"), "{err}");
        Ok(())
    }

    /// A swap that landed and reverted is `Done` too, with the replayed reason
    /// and no amounts: it took nothing and paid nothing.
    #[tokio::test]
    async fn a_timed_out_swap_found_reverted_is_done_with_its_reason() -> Result<()> {
        let (server, live, tx_hash, no_receipt) = timed_out_swap().await?;
        drop(no_receipt);
        mount_receipt(
            &server,
            json!({
                "status": "0x0", "blockNumber": "0x2b", "logs": [],
                "gasUsed": "0x249f0", "effectiveGasPrice": "0x77359400",
            }),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": 3, "message": "execution reverted",
                           "data": hex_data(&encode_error_string("Too little received")) },
            })))
            .mount(&server)
            .await;

        let Resolution::Done(realised) = live.resolve(tx_hash).await? else {
            bail!("a mined swap is Done");
        };

        assert!(
            matches!(&realised.outcome, Outcome::Reverted { reason } if reason == "Too little received"),
            "{:?}",
            realised.outcome
        );
        assert_eq!((realised.amount_in, realised.amount_out), (None, None));
        assert_eq!(realised.at, 0x2b);
        assert_eq!(realised.tx_ref, Some(tx_hash.as_slice().to_vec()));
        assert_eq!(live.sender().unresolved(), None);
        Ok(())
    }

    /// `Gone` is for a transaction the node no longer knows whose nonce was
    /// used. Until the nonce is used another node may yet broadcast it, so the
    /// swap is still pending.
    #[tokio::test]
    async fn a_swap_replaced_or_dropped_is_gone() -> Result<()> {
        let (server, live, tx_hash, _no_receipt) = timed_out_swap().await?;

        // The node forgets it, but its nonce (5) is unused: still pending.
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionByHash"}),
            ))
            .respond_with(rpc_ok(Value::Null))
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(matches!(live.resolve(tx_hash).await?, Resolution::Pending));
        assert_eq!(live.sender().unresolved(), Some(tx_hash));

        // Something else took nonce 5.
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionCount", "params": [live.address().to_string(), "latest"]}),
            ))
            .respond_with(rpc_ok(json!("0x6")))
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(matches!(live.resolve(tx_hash).await?, Resolution::Gone));

        // Forgotten, and the sender is free to send again.
        assert_eq!(live.sender().unresolved(), None);
        assert!(live.resolve(tx_hash).await.is_err());
        Ok(())
    }

    /// A failed read says nothing about the swap: it is an error, never `Gone`
    /// and never a result, and the swap stays resolvable by the same call.
    #[tokio::test]
    async fn an_rpc_error_while_resolving_is_an_error_and_keeps_the_swap() -> Result<()> {
        let (server, live, tx_hash, no_receipt) = timed_out_swap().await?;

        drop(no_receipt);
        let failing = Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32005, "message": "rate limit exceeded" },
            })))
            .mount_as_scoped(&server)
            .await;
        for _ in 0..2 {
            let err = live.resolve(tx_hash).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("rate limit exceeded"),
                "{err:#}"
            );
            assert!(err.downcast_ref::<TxReplacedOrDropped>().is_none());
            assert_eq!(live.sender().unresolved(), Some(tx_hash));
        }

        // The node answers again: the same swap resolves.
        drop(failing);
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2b", "logs": pool_took_600(live.address()) }),
        )
        .await;
        assert!(matches!(
            live.resolve(tx_hash).await?,
            Resolution::Done(realised) if realised.amount_in == Some(600)
        ));
        Ok(())
    }

    /// A swap that landed with logs the decoders cannot read is an error, as it
    /// is from `execute`, and it is one answer: the sender has handed over the
    /// outcome and cleared its latch, so the swap is decided and forgotten, and a
    /// second ask says there is nothing to resolve, not that something else
    /// resolved it.
    #[tokio::test]
    async fn a_timed_out_swap_that_lands_undecodable_is_an_error_once_and_forgotten() -> Result<()>
    {
        let (server, live, tx_hash, no_receipt) = timed_out_swap().await?;
        drop(no_receipt);
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2b", "logs": [] }),
        )
        .await;

        let err = live.resolve(tx_hash).await.unwrap_err();
        assert!(
            err.to_string().contains("no ERC-20 Transfer")
                && err.to_string().contains(&tx_hash.to_string()),
            "{err}"
        );
        assert_eq!(live.sender().unresolved(), None);

        let again = live.resolve(tx_hash).await.unwrap_err();
        assert!(again.to_string().contains("nothing to resolve"), "{again}");
        assert!(live.timed_out.lock().unwrap().is_empty());
        Ok(())
    }

    /// A hash this adapter did not time out is not its to resolve, and asking
    /// leaves the swap it did time out untouched.
    #[tokio::test]
    async fn a_hash_this_adapter_did_not_time_out_is_an_error() -> Result<()> {
        let (_server, live, tx_hash, _no_receipt) = timed_out_swap().await?;

        let err = live.resolve(B256::repeat_byte(0x07)).await.unwrap_err();

        assert!(err.to_string().contains("did not time out"), "{err}");
        assert_eq!(live.sender().unresolved(), Some(tx_hash));
        assert!(matches!(live.resolve(tx_hash).await?, Resolution::Pending));
        Ok(())
    }

    /// Resolving the sender directly hands the outcome to its caller; the swap
    /// is then not this adapter's to read, and says so.
    #[tokio::test]
    async fn a_swap_the_sender_was_resolved_for_directly_is_an_error_here() -> Result<()> {
        let (server, live, tx_hash, no_receipt) = timed_out_swap().await?;
        drop(no_receipt);
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2b", "logs": [] }),
        )
        .await;
        assert!(live.sender().resolve().await?.is_some());

        let err = live.resolve(tx_hash).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("resolved through the sender directly"),
            "{err}"
        );
        Ok(())
    }

    /// The signing sender prices a transaction by its fee policy alone, and
    /// `EvmLive` refuses a bid above it by name, never sending without it. A
    /// bid of nothing is the policy.
    #[tokio::test]
    async fn a_bid_above_the_fee_policy_is_refused_by_name_over_a_signing_sender() -> Result<()> {
        let (server, live) = offline_live().await;
        let route = route(ROUTER, TOKEN_IN, TOKEN_OUT, &[0xCA, 0xFE]);
        let bid = |priority| SwapRequest {
            priority,
            ..request(live.address(), RECIPIENT)
        };

        live.prepare(&route, &bid(PriorityBid::AbovePolicyPerGas(0)))
            .await?;
        let err = live
            .prepare(&route, &bid(PriorityBid::AbovePolicyPerGas(1)))
            .await
            .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("EvmLive") && message.contains("fee policy only"),
            "{message}"
        );
        let sent = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.body_json::<Value>()
                    .is_ok_and(|b| b["method"] == "eth_sendRawTransaction")
            })
            .count();
        assert_eq!(sent, 0);
        Ok(())
    }

    /// Runtime code that, called with anything, emits two ERC-20 `Transfer`s of
    /// itself: 1 000 from itself to `pool`, and 900 from `pool` to `recipient`.
    /// With `Payer::CalledContract`, a swap through it takes 1 000 and pays 900.
    fn two_transfer_code(this: Address, pool: Address, recipient: Address) -> Vec<u8> {
        let mut code = Vec::new();
        for (from, to, value) in [(this, pool, 1_000u16), (pool, recipient, 900)] {
            code.extend([0x61, (value >> 8) as u8, value as u8]); // PUSH2 value
            code.extend([0x60, 0x00, 0x52]); // PUSH1 0, MSTORE: memory[0..32] = value
            for word in [pad_address(to), pad_address(from), transfer_topic().0] {
                code.push(0x7f); // PUSH32 topic2, then topic1, then topic0
                code.extend(word);
            }
            code.extend([0x60, 0x20, 0x60, 0x00]); // PUSH1 32 (size), PUSH1 0 (offset)
            code.push(0xa3); // LOG3
        }
        code.push(0x00); // STOP
        code
    }

    /// The whole road against a real node, in the shape a consumer meets it: with
    /// automine off a swap through a signing sender times out and is `Simulated`
    /// (the node is anvil); it is pending while nothing is mined, and `Done` with
    /// what the contract took and paid once a block is.
    ///
    /// Gated on `EVM_ANVIL_RPC_URL`, a plain anvil with no block time; a no-op
    /// when unset.
    #[tokio::test]
    async fn against_anvil_a_timed_out_swap_is_pending_then_done_with_its_amounts() -> Result<()> {
        use crate::evm::tx::tests::{anvil, ANVIL_DEV_KEY_0};

        let Some((rpc, _anvil)) = anvil().await else {
            return Ok(());
        };
        let contract = Address::new([0xC1; 20]);
        rpc.call(
            "anvil_setCode",
            json!([
                contract.to_string(),
                hex_data(&two_transfer_code(contract, POOL, RECIPIENT))
            ]),
        )
        .await?;

        let sender = EvmSender::connect(
            rpc.clone(),
            Signer::from_private_key_hex(ANVIL_DEV_KEY_0)?,
            rpc.chain_id().await?,
            FeePolicy::default(),
        )
        .await?;
        sender.set_poll_settings(PollSettings {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
        });
        let live = EvmLive::new(sender);
        let prepared = live
            .prepare(
                &route(contract, contract, contract, &[0xCA, 0xFE]),
                &SwapRequest {
                    payer: Payer::CalledContract,
                    ..request(live.address(), RECIPIENT)
                },
            )
            .await?;

        rpc.call("evm_setAutomine", json!([false])).await?;
        let timed_out = live.execute(&prepared, None).await;
        let pending = match &timed_out {
            Ok(realised) => match &realised.tx_ref {
                Some(tx_ref) => Some(live.resolve(B256::try_from(tx_ref.as_slice())?).await),
                None => None,
            },
            Err(_) => None,
        };
        rpc.call("evm_mine", json!([])).await?;
        rpc.call("evm_setAutomine", json!([true])).await?;

        let timed_out = timed_out?;
        assert!(
            matches!(timed_out.outcome, Outcome::TimedOut),
            "{timed_out:?}"
        );
        assert_eq!(timed_out.provenance, Provenance::Simulated);
        let tx_hash = B256::try_from(
            timed_out
                .tx_ref
                .ok_or_else(|| anyhow!("a timeout names its hash"))?
                .as_slice(),
        )?;
        let pending = pending.ok_or_else(|| anyhow!("the swap was asked about"))?;
        assert!(matches!(pending?, Resolution::Pending));

        let Resolution::Done(realised) = live.resolve(tx_hash).await? else {
            bail!("the swap was mined");
        };
        assert!(matches!(realised.outcome, Outcome::Success), "{realised:?}");
        assert_eq!(realised.amount_in, Some(1_000));
        assert_eq!(realised.amount_out, Some(900));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, Some(tx_hash.as_slice().to_vec()));
        assert_eq!(live.sender().unresolved(), None);
        Ok(())
    }

    /// Real Sepolia, all three legs SPEC.md §3 describes for a DEX port,
    /// same pool, same input size: **quoted** (Uniswap's own `QuoterV2` —
    /// independent of this crate), **simulated** (`EvmSimulated`'s
    /// `eth_call` + state overrides), and **executed** (`EvmLive`, this
    /// module — a real signed swap). No-ops when the real-network env vars
    /// aren't set, same as every other real-Sepolia test in this crate.
    ///
    /// The signer's wallet holds only Sepolia ETH going in, so step 0
    /// wraps a small amount into WETH first (`WETH9.deposit()`) — outside
    /// `DexExecutor` entirely, since funding an address is explicitly not
    /// this crate's job (`SPEC.md` §2). That wrap, and the swap's own
    /// `approve`, both go out through the same `EvmSender` the swap uses,
    /// not a reimplementation of it.
    #[tokio::test]
    async fn real_sepolia_quote_vs_simulated_vs_executed() {
        let (Ok(rpc_url), Ok(signer_key)) = (
            std::env::var("EVM_LIVE_RPC_URL"),
            std::env::var("EVM_LIVE_SIGNER_KEY"),
        ) else {
            eprintln!("skipping: EVM_LIVE_RPC_URL / EVM_LIVE_SIGNER_KEY not set");
            return;
        };

        let weth: Address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
            .parse()
            .unwrap();
        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        let quoter: Address = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
            .parse()
            .unwrap();
        const FEE: u64 = 3_000; // 0.3%
        const AMOUNT_IN: u128 = 1_000_000_000_000_000; // 0.001 WETH

        let rpc = EvmRpc::new(rpc_url);
        let sender = EvmSender::connect(
            rpc.clone(),
            Signer::from_private_key_hex(&signer_key).unwrap(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .unwrap();
        let live = EvmLive::new(sender.clone());
        let me = live.address();

        // Step 0: make sure there's real WETH to swap.
        let weth_balance = erc20::balance_of(&rpc, weth, me, crate::evm::BlockTag::Latest)
            .await
            .expect("checking WETH balance");
        if weth_balance < U256::from(AMOUNT_IN) {
            let deposit_calldata = vec![0xd0, 0xe3, 0x0d, 0xb0]; // WETH9.deposit()
            match sender
                .send_and_confirm(
                    weth,
                    deposit_calldata,
                    U256::from(AMOUNT_IN) * U256::from(4u64),
                )
                .await
                .expect("sending the wrap transaction")
            {
                TxOutcome::Success { tx_hash, .. } => {
                    eprintln!("wrapped ETH -> WETH: tx {tx_hash}")
                }
                TxOutcome::Reverted { reason, .. } => panic!("WETH deposit() reverted: {reason}"),
                TxOutcome::TimedOut { tx_hash } => {
                    panic!("WETH deposit() timed out (tx {tx_hash})")
                }
            }
        }

        // Leg 1: quoted — Uniswap's own QuoterV2, not this crate.
        let mut quote_calldata = vec![0xc6, 0xa5, 0x02, 0x6a];
        quote_calldata.extend_from_slice(&pad_address(weth));
        quote_calldata.extend_from_slice(&pad_address(usdc));
        quote_calldata.extend_from_slice(&U256::from(AMOUNT_IN).to_be_bytes::<32>());
        quote_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
        quote_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        let quote_bytes = rpc
            .eth_call(
                quoter,
                &quote_calldata,
                None,
                crate::evm::BlockTag::Latest,
                None,
            )
            .await
            .expect("quoter call should succeed");
        let quoted_amount_out: u128 = crate::evm::rpc::first_word(&quote_bytes)
            .unwrap()
            .try_into()
            .unwrap();

        // The shared swap calldata both Simulated and Executed run —
        // amountOutMinimum left at 0, same as this crate's other
        // exploratory real-network tests, to avoid a slippage revert on a
        // thin real pool.
        let mut swap_calldata = vec![0x04, 0xe4, 0x5a, 0xaf]; // exactInputSingle
        swap_calldata.extend_from_slice(&pad_address(weth));
        swap_calldata.extend_from_slice(&pad_address(usdc));
        swap_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&pad_address(me));
        swap_calldata.extend_from_slice(&U256::from(AMOUNT_IN).to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(&swap_calldata);

        let route = RouteQuote {
            network: Network::evm(SEPOLIA),
            token_in: weth.as_slice().to_vec(),
            token_out: usdc.as_slice().to_vec(),
            amount_in: AMOUNT_IN,
            expected_amount_out: quoted_amount_out,
            payload,
        };

        // Leg 2: simulated — this crate's EvmSimulated, eth_call + state
        // overrides, nothing broadcast.
        let simulated = crate::dex::evm::EvmSimulated::new(rpc.clone());
        let sim_request = SwapRequest {
            sender: me.as_slice().to_vec(),
            recipient: me.as_slice().to_vec(),
            payer: Payer::Sender,
            min_amount_out: 0,
            deadline_unix_secs: 0,
            priority: PriorityBid::Policy,
        };
        let sim_prepared = simulated.prepare(&route, &sim_request).await.unwrap();
        let sim_realised = simulated.execute(&sim_prepared, None).await.unwrap();

        // Leg 3: executed — this crate's EvmLive, a real signed swap
        // through the real router.
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let live_request = SwapRequest {
            sender: me.as_slice().to_vec(),
            recipient: me.as_slice().to_vec(),
            payer: Payer::Sender,
            min_amount_out: 0,
            deadline_unix_secs: deadline,
            priority: PriorityBid::Policy,
        };
        let live_prepared = live.prepare(&route, &live_request).await.unwrap();
        let live_realised = live.execute(&live_prepared, None).await.unwrap();

        eprintln!(
            "\nWETH->USDC, amountIn = {AMOUNT_IN} wei WETH (0.001 WETH)\n\
             {:>10} | {:>18} wei USDC\n\
             {:>10} | {:?} (outcome {:?})\n\
             {:>10} | {:?} (outcome {:?}, tx {})\n",
            "quoted",
            quoted_amount_out,
            "simulated",
            sim_realised.amount_out,
            sim_realised.outcome,
            "executed",
            live_realised.amount_out,
            live_realised.outcome,
            live_realised
                .tx_ref
                .as_ref()
                .map(|h| format!("0x{}", hex::encode(h)))
                .unwrap_or_default(),
        );

        assert!(matches!(sim_realised.outcome, Outcome::Success));
        assert!(matches!(live_realised.outcome, Outcome::Success));
        assert_eq!(live_realised.provenance, Provenance::Landed);
        assert!(live_realised.tx_ref.is_some());
    }
}
