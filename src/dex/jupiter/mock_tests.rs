//! Jupiter's adapters against a mock node: one server that answers the Solana
//! JSON-RPC calls a swap makes and Jupiter's own `/swap`, so what an adapter
//! reports can be tested without a Surfpool fork (`surfpool_tests` runs the
//! same adapters on one, when `SURFPOOL_RPC_URL` is set).

use crate::dex::jupiter::{JupiterConfig, JupiterLive, JupiterSimulated};
use crate::dex::{DexExecutor, Outcome, Payer, Prepared, PriorityBid, RouteQuote, SwapRequest};
use crate::solana::token::{associated_token_account, TOKEN_PROGRAM};
use crate::solana::{SolanaRpc, SolanaSender};
use crate::testkit::contract::{dex_executor_contract, DexContractFixture, Sends};
use crate::{Network, Provenance};
use base64::Engine;
use serde_json::{json, Value};
use solana_address::Address;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const IN_AMOUNT: u64 = 1_000_000;
const OUT_AMOUNT: u64 = 900_000;
/// What the destination holds after the swap, when it ran.
const RECEIVED: u64 = 880_000;
const CLOCK_SYSVAR: &str = "SysvarC1ock11111111111111111111111111111111";

fn address(byte: u8) -> Address {
    Address::new_from_array([byte; 32])
}

fn in_mint() -> Address {
    address(1)
}

fn out_mint() -> Address {
    address(2)
}

fn genesis() -> [u8; 32] {
    [9; 32]
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// An account as `getMultipleAccounts` and `simulateTransaction` give it.
fn account(owner: &Address, data: &[u8]) -> Value {
    json!({
        "lamports": 2_039_280,
        "owner": owner.to_string(),
        "data": [base64(data), "base64"],
        "executable": false,
        "rentEpoch": 0,
    })
}

/// A token account holding `amount`: the amount is at bytes 64 to 72.
fn token_account(amount: u64) -> Value {
    let mut data = vec![0u8; 165];
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    account(&TOKEN_PROGRAM, &data)
}

/// What the mock node does with the swap.
#[derive(Clone)]
struct Venue {
    /// The error the swap fails with, if it does (in a dry run and on the fork).
    fails_with: Option<Value>,
    /// The sender's address, once the fork sender exists: the destination of
    /// the swap's proceeds is its token account.
    owner: Arc<Mutex<Option<Address>>>,
}

impl Venue {
    fn new(fails_with: Option<Value>) -> Self {
        Self {
            fails_with,
            owner: Arc::new(Mutex::new(None)),
        }
    }

    fn answer(&self, rpc_method: &str, params: &Value) -> Value {
        match rpc_method {
            "getGenesisHash" => json!(Address::new_from_array(genesis()).to_string()),
            "surfnet_getSurfnetInfo" => json!({}),
            "surfnet_setAccount" => Value::Null,
            "getLatestBlockhash" => json!({
                "context": { "slot": 1 },
                "value": {
                    "blockhash": address(4).to_string(),
                    "lastValidBlockHeight": 50,
                },
            }),
            "getBlockHeight" => json!(10),
            "getMultipleAccounts" => {
                let keys = params[0].as_array().unwrap();
                let values: Vec<Value> = keys
                    .iter()
                    .map(|key| match key.as_str().unwrap() {
                        key if key == out_mint().to_string() => account(&TOKEN_PROGRAM, &[0; 82]),
                        // The clock is far ahead of the wall clock: a fork's
                        // accounts are always loaded by then.
                        CLOCK_SYSVAR => {
                            let mut clock = vec![0u8; 40];
                            clock[32..40].copy_from_slice(&(i64::MAX / 2).to_le_bytes());
                            account(
                                &"Sysvar1111111111111111111111111111111111111"
                                    .parse()
                                    .unwrap(),
                                &clock,
                            )
                        }
                        // The destination holds nothing before the swap.
                        _ => Value::Null,
                    })
                    .collect();
                json!({ "context": { "slot": 1 }, "value": values })
            }
            "simulateTransaction" => {
                let asked = params[1]["accounts"]["addresses"]
                    .as_array()
                    .map_or(0, Vec::len);
                json!({
                    "context": { "slot": 7 },
                    "value": {
                        "err": self.fails_with,
                        "logs": [],
                        "unitsConsumed": 1_234,
                        "accounts": vec![token_account(RECEIVED); asked],
                    },
                })
            }
            "sendTransaction" => json!("sig"),
            "getSignatureStatuses" => json!({
                "context": { "slot": 8 },
                "value": [{ "slot": 8, "err": self.fails_with, "confirmationStatus": "confirmed" }],
            }),
            "getTransaction" => {
                let owner = self.owner.lock().unwrap().expect("the sender exists");
                let destination = associated_token_account(&owner, &out_mint(), &TOKEN_PROGRAM);
                json!({
                    "slot": 8,
                    "transaction": { "message": { "accountKeys": [
                        owner.to_string(),
                        destination.to_string(),
                    ] } },
                    "meta": {
                        "err": self.fails_with,
                        "fee": 5_000,
                        "computeUnitsConsumed": 1_234,
                        "preBalances": [1, 1],
                        "postBalances": [1, 1],
                        "preTokenBalances": [],
                        "postTokenBalances": [{
                            "accountIndex": 1,
                            "mint": out_mint().to_string(),
                            "owner": owner.to_string(),
                            "uiTokenAmount": { "amount": RECEIVED.to_string() },
                        }],
                        "logMessages": [],
                    },
                })
            }
            other => panic!("unexpected {other}"),
        }
    }

    /// A node that answers the JSON-RPC at `/` and Jupiter's `/swap`.
    async fn serve(&self) -> MockServer {
        let server = MockServer::start().await;
        let venue = self.clone();
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let result = venue.answer(body["method"].as_str().unwrap(), &body["params"]);
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(&server)
            .await;
        // Jupiter builds an unsigned transaction whose fee payer is the user.
        Mock::given(method("POST"))
            .and(path("/swap"))
            .respond_with(|req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let user: Address = body["userPublicKey"].as_str().unwrap().parse().unwrap();
                let Prepared::Solana(built) = Prepared::offline(
                    Network::Solana {
                        genesis_hash: genesis(),
                    },
                    &user.to_bytes(),
                    0,
                ) else {
                    unreachable!("a Solana network is prepared as a Solana transaction")
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "swapTransaction": base64(&bincode::serialize(&built.transaction).unwrap()),
                    "lastValidBlockHeight": 50,
                    "prioritizationFeeLamports": 1_000,
                }))
            })
            .mount(&server)
            .await;
        server
    }
}

fn config(server: &MockServer) -> JupiterConfig {
    JupiterConfig {
        base_url: server.uri(),
        api_key: None,
        prioritization_fee_lamports: Some(1_000),
    }
}

/// A Jupiter `ExactIn` quote of `IN_AMOUNT` for `OUT_AMOUNT`, as the route's
/// payload.
fn route(network: Network) -> RouteQuote {
    let quote = json!({
        "inputMint": in_mint().to_string(),
        "inAmount": IN_AMOUNT.to_string(),
        "outputMint": out_mint().to_string(),
        "outAmount": OUT_AMOUNT.to_string(),
        "otherAmountThreshold": "0",
        "swapMode": "ExactIn",
        "slippageBps": 50,
        "routePlan": [],
    });
    RouteQuote {
        network,
        token_in: in_mint().to_bytes().to_vec(),
        token_out: out_mint().to_bytes().to_vec(),
        amount_in: u128::from(IN_AMOUNT),
        expected_amount_out: u128::from(OUT_AMOUNT),
        payload: serde_json::to_vec(&quote).unwrap(),
    }
}

fn request(owner: &Address) -> SwapRequest {
    SwapRequest {
        sender: owner.to_bytes().to_vec(),
        recipient: owner.to_bytes().to_vec(),
        payer: Payer::Sender,
        // 50 bps below the quote.
        min_amount_out: 895_500,
        deadline_unix_secs: crate::liquidity::unix_now() + 300,
        priority: PriorityBid::Policy,
    }
}

/// An `ExactIn` swap that ran took the whole offer, and a dry run that
/// failed took none.
#[tokio::test]
async fn a_simulated_swap_reports_its_offer_as_taken_and_a_failed_one_none() {
    let owner = address(7);

    let venue = Venue::new(None);
    let server = venue.serve().await;
    let adapter = JupiterSimulated::connect(SolanaRpc::new(server.uri()), config(&server))
        .await
        .unwrap();
    let fixture = DexContractFixture {
        route: route(Network::Solana {
            genesis_hash: genesis(),
        }),
        request: request(&owner),
    };
    let prepared = adapter
        .prepare(&fixture.route, &fixture.request)
        .await
        .unwrap();
    let realised = adapter.execute(&prepared, None).await.unwrap();
    assert!(matches!(realised.outcome, Outcome::Success), "{realised:?}");
    assert_eq!(realised.amount_out, Some(u128::from(RECEIVED)));
    assert_eq!(realised.amount_in, Some(u128::from(IN_AMOUNT)));
    assert_eq!(realised.provenance, Provenance::Simulated);
    dex_executor_contract(&adapter, Sends::Nothing, fixture).await;

    let venue = Venue::new(Some(json!({ "InstructionError": [0, { "Custom": 6001 }] })));
    let server = venue.serve().await;
    let adapter = JupiterSimulated::connect(SolanaRpc::new(server.uri()), config(&server))
        .await
        .unwrap();
    let prepared = adapter
        .prepare(
            &route(Network::Solana {
                genesis_hash: genesis(),
            }),
            &request(&owner),
        )
        .await
        .unwrap();
    let realised = adapter.execute(&prepared, None).await.unwrap();
    assert!(
        matches!(realised.outcome, Outcome::Reverted { .. }),
        "{realised:?}"
    );
    assert_eq!(realised.amount_out, None);
    assert_eq!(realised.amount_in, None);
}

/// The same over a fork sender: a swap that landed took the whole offer, one
/// that failed on the fork took none.
#[tokio::test]
async fn a_swap_sent_to_a_fork_reports_its_offer_as_taken_and_a_failed_one_none() {
    for fails_with in [
        None,
        Some(json!({ "InstructionError": [0, { "Custom": 6001 }] })),
    ] {
        let venue = Venue::new(fails_with.clone());
        let server = venue.serve().await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .unwrap();
        *venue.owner.lock().unwrap() = Some(sender.pubkey());
        let adapter = JupiterLive::new(sender.clone(), config(&server));

        let fixture = DexContractFixture {
            route: route(sender.network()),
            request: request(&sender.pubkey()),
        };
        let prepared = adapter
            .prepare(&fixture.route, &fixture.request)
            .await
            .unwrap();
        let realised = adapter.execute(&prepared, None).await.unwrap();
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert!(realised.tx_ref.is_some());
        if fails_with.is_none() {
            assert!(matches!(realised.outcome, Outcome::Success), "{realised:?}");
            assert_eq!(realised.amount_out, Some(u128::from(RECEIVED)));
            assert_eq!(realised.amount_in, Some(u128::from(IN_AMOUNT)));
            dex_executor_contract(&adapter, Sends::Transactions, fixture).await;
        } else {
            assert!(
                matches!(realised.outcome, Outcome::Reverted { .. }),
                "{realised:?}"
            );
            assert_eq!(realised.amount_out, None);
            assert_eq!(realised.amount_in, None);
        }
    }
}
