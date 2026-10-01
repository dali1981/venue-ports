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
//! **Over a fork sender** (`SPEC.md` §5b), this adapter is a swap simulator
//! whose state persists between calls: its provenance is `Simulated` and it
//! sets no `tx_ref`. The owner must hold the input token on the fork;
//! [`EvmSender::ensure_balance`] writes it there.

use crate::dex::{
    ChainAmount, DexExecutor, EvmCall, EvmCost, Outcome, Prepared, Realised, RouteQuote,
    SwapRequest, TxCost,
};
use crate::evm::erc20;
use crate::evm::rpc::address_from_slice;
use crate::evm::{prepared_key, EvmSender, RpcLog, TxOutcome};
use crate::Network;
use crate::Provenance;
use alloy_primitives::{Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct PendingLive {
    token_in: Address,
    token_out: Address,
    recipient: Address,
    amount_in: ChainAmount,
    deadline_unix_secs: u64,
}

pub struct EvmLive {
    sender: Arc<EvmSender>,
    pending: Mutex<HashMap<B256, PendingLive>>,
}

impl EvmLive {
    pub fn new(sender: Arc<EvmSender>) -> Self {
        Self {
            sender,
            pending: Mutex::new(HashMap::new()),
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

    /// `tx_ref` is set if and only if the provenance is `Landed`.
    fn tx_ref(&self, tx_hash: B256) -> Option<Vec<u8>> {
        (self.sender.provenance() == Provenance::Landed).then(|| tx_hash.as_slice().to_vec())
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
                recipient,
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

        self.sender
            .ensure_allowance(ctx.token_in, router, U256::from(ctx.amount_in))
            .await?;

        match self
            .sender
            .send_and_confirm(router, prepared.calldata.clone(), U256::ZERO)
            .await?
        {
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
                Ok(Realised {
                    amount_out: Some(amount_out),
                    outcome: Outcome::Success,
                    cost: TxCost::Evm(cost),
                    at: block,
                    provenance: self.sender.provenance(),
                    tx_ref: self.tx_ref(tx_hash),
                })
            }
            TxOutcome::Reverted {
                block,
                tx_hash,
                reason,
                cost,
            } => Ok(Realised {
                amount_out: None,
                outcome: Outcome::Reverted { reason },
                cost: TxCost::Evm(cost),
                at: block,
                provenance: self.sender.provenance(),
                tx_ref: self.tx_ref(tx_hash),
            }),
            TxOutcome::TimedOut { tx_hash } => {
                let at = self.sender.rpc().block_number().await.unwrap_or(0);
                Ok(Realised {
                    amount_out: None,
                    outcome: Outcome::TimedOut,
                    cost: TxCost::Evm(EvmCost::default()),
                    at,
                    provenance: self.sender.provenance(),
                    tx_ref: self.tx_ref(tx_hash),
                })
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::erc20::{transfer_topic, ALLOWANCE_SELECTOR};
    use crate::evm::rpc::{encode_error_string, format_u256, hex_data, pad_address};
    use crate::evm::tx::tests::{connect, mount_receipt, mount_send_plumbing, SEPOLIA};
    use crate::evm::{EvmRpc, FeePolicy, PollSettings, Signer};
    use serde_json::{json, Value};
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
            min_amount_out: 900,
            deadline_unix_secs: 9_999_999_999,
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

        let transfer_log = json!({
            "address": token_out.to_string(),
            "topics": [
                transfer_topic().to_string(),
                hex_data(&pad_address(router)),
                hex_data(&pad_address(recipient)),
            ],
            "data": format_u256(U256::from(4_200u64)),
        });
        mount_receipt(
            &server,
            json!({
                "status": "0x1", "blockNumber": "0x2a", "logs": [transfer_log],
                "gasUsed": "0x249f0", "effectiveGasPrice": "0x77359400", "l1Fee": "0x7",
            }),
        )
        .await;

        let live = EvmLive::new(connect(&server).await);
        let sender = live.address();
        let route = route(router, token_in, token_out, &calldata);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, Some(4_200));
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

    #[tokio::test]
    async fn satisfies_the_dex_contract() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token_out = Address::from([0xBB; 20]);
        let recipient = Address::from([0xDD; 20]);
        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;
        let transfer_log = json!({
            "address": token_out.to_string(),
            "topics": [
                transfer_topic().to_string(),
                hex_data(&pad_address(router)),
                hex_data(&pad_address(recipient)),
            ],
            "data": format_u256(U256::from(950u64)),
        });
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2a", "logs": [transfer_log] }),
        )
        .await;

        let live = EvmLive::new(connect(&server).await);
        let fixture = crate::testkit::contract::DexContractFixture {
            route: route(router, Address::from([0xAA; 20]), token_out, &[0xCA, 0xFE]),
            request: request(live.address(), recipient),
        };
        crate::testkit::contract::dex_executor_contract(&live, fixture).await;
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
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        match realised.outcome {
            Outcome::Reverted { reason: got } => assert_eq!(got, reason),
            other => panic!("expected Reverted, got {other:?}"),
        }
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
            min_amount_out: 0,
            deadline_unix_secs: 0,
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
            min_amount_out: 0,
            deadline_unix_secs: deadline,
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
