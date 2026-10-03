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
//! of the router's return data. `SwapRouter02.exactInputSingle` returns one
//! word; KyberSwap's `MetaAggregationRouterV2.swap` returns
//! `(uint256 returnAmount, uint256 gasUsed)`, and the other common
//! aggregator routers likewise put the output amount first. Fewer than 32
//! bytes is an error, never a panic. A router whose output is not its first
//! word needs its own adapter.

use crate::dex::{
    ChainAmount, DexExecutor, EvmCall, EvmCost, Outcome, Payer, Prepared, Realised, RouteQuote,
    SwapRequest, TxCost,
};
use crate::evm::erc20::{self, SlotCache};
use crate::evm::prepared_key;
use crate::evm::rpc::{
    address_from_slice, first_word, format_u256, hex_data, BlockTag, EvmRpc, RpcError,
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

pub struct EvmSimulated {
    rpc: EvmRpc,
    pending: Mutex<HashMap<B256, PendingSimulation>>,
    slots: SlotCache,
    code: HashMap<Address, CodeOverride>,
}

impl EvmSimulated {
    pub fn new(rpc: EvmRpc) -> Self {
        Self {
            rpc,
            pending: Mutex::new(HashMap::new()),
            slots: SlotCache::default(),
            code: HashMap::new(),
        }
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
        let balance_index =
            erc20::find_balance_slot(&self.rpc, &self.slots, ctx.token_in, holder, tag).await?;
        let balance_slot = erc20::mapping_slot(holder, balance_index);
        set_slot(&mut overrides, ctx.token_in, balance_slot, &amount_in);

        match ctx.payer {
            Payer::Sender => {
                let allowance_index = erc20::find_allowance_slot(
                    &self.rpc,
                    &self.slots,
                    ctx.token_in,
                    ctx.sender,
                    router,
                    tag,
                )
                .await?;
                let allowance_slot = erc20::allowance_slot(ctx.sender, router, allowance_index);
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

        let block = match at {
            Some(block) => block,
            None => self.rpc.block_number().await?,
        };
        let tag = BlockTag::Number(block);
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
                let amount_out: u128 = first_word(&data)
                    .context("decoding the router's return data")?
                    .try_into()
                    .context("amount_out overflowed u128")?;
                Ok(Realised {
                    amount_out: Some(amount_out),
                    outcome: Outcome::Success,
                    // An `eth_call` reports no gas.
                    cost: TxCost::Evm(EvmCost::default()),
                    at: block,
                    provenance: Provenance::Simulated,
                    tx_ref: None,
                })
            }
            Err(err) => match err.downcast::<RpcError>() {
                Ok(revert) => Ok(Realised {
                    amount_out: None,
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
        let sender = Address::from([0xCC; 20]);
        mount_slot_probe(server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;
        mount_slot_probe(
            server,
            ALLOWANCE_SELECTOR,
            allowance_slot(sender, Address::from(router), 0),
        )
        .await;
        mount_swap(server, calldata, swap_return).await;
    }

    /// A swap call (calldata exactly `calldata`) that returns `swap_return`;
    /// any other `eth_call` gets zero.
    async fn mount_swap(server: &MockServer, calldata: &[u8], swap_return: Vec<u8>) {
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
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
        assert_eq!(realised.at, 42);

        // The allowance override must have gone to the router the route
        // actually calls (SPEC.md §5's explicit pitfall), not some other
        // address.
        assert_eq!(adapter.slots.allowance_index(token, router_addr), Some(0));
    }

    /// Every `eth_call` a mock node received, as its JSON body.
    async fn eth_calls(server: &MockServer) -> Vec<Json> {
        server
            .received_requests()
            .await
            .expect("the mock records its requests")
            .iter()
            .map(|r| r.body_json::<Json>().unwrap())
            .filter(|body| body["method"] == "eth_call")
            .collect()
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
            "real Sepolia USDC @ {block:?}: balanceOf slot index {balance_index}, \
             allowance slot index {allowance_index}"
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
