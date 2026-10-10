//! Test scaffolding shared by the harness's own tests and the self-tests of the
//! cases: the settings of a run in a scratch directory, short timings, and a
//! mock venue's clock.

use crate::cex::CexTimings;
use crate::production::env::MapEnv;
use crate::production::gate::Echo;
use crate::production::Run;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub(crate) fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "venue-ports-run-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

pub(crate) fn fast() -> CexTimings {
    CexTimings {
        request_timeout: Duration::from_millis(500),
        recv_window: Duration::from_millis(1_000),
        clock_refresh: Duration::from_secs(600),
        poll_interval: Duration::from_millis(10),
        poll_timeout: Duration::from_millis(300),
        trades_timeout: Duration::from_millis(200),
    }
}

/// The settings of a run against `server`, in a scratch directory.
pub(crate) struct Setup {
    pub(crate) vars: Vec<(String, String)>,
    pub(crate) out: PathBuf,
    pub(crate) halt: PathBuf,
    pub(crate) record: PathBuf,
}

impl Setup {
    pub(crate) fn new(name: &str, server: &MockServer) -> Self {
        let root = scratch(name);
        let (out, halt, record) = (root.join("out"), root.join("HALT"), root.join("recording"));
        let vars = [
            ("VP_SPEND_CAP_USD", "30".to_string()),
            ("VP_HALT_FILE", halt.display().to_string()),
            ("VP_OUT", out.display().to_string()),
            ("BINANCE_BASE_URL", server.uri()),
            ("BINANCE_API_KEY", "AKEY-1234-ABCD".to_string()),
            ("BINANCE_API_SECRET", "SECRET-9876-WXYZ".to_string()),
        ]
        .map(|(key, value)| (key.to_string(), value))
        .to_vec();
        Self {
            vars,
            out,
            halt,
            record,
        }
    }

    pub(crate) fn with(mut self, key: &str, value: &str) -> Self {
        self.vars.retain(|(name, _)| name != key);
        self.vars.push((key.to_string(), value.to_string()));
        self
    }

    pub(crate) fn without(mut self, key: &str) -> Self {
        self.vars.retain(|(name, _)| name != key);
        self
    }

    pub(crate) fn env(&self) -> MapEnv {
        let vars: Vec<(&str, &str)> = self
            .vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        MapEnv::new(&vars)
    }

    pub(crate) fn run(&self, server: &MockServer) -> Result<Run> {
        Run::start(&self.env(), "binance-spot", &server.uri(), Echo::Quiet)
    }

    pub(crate) fn lines(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.out.join("results.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub(crate) fn clean_up(&self) {
        let _ = std::fs::remove_dir_all(self.out.parent().unwrap());
    }
}

/// Which asks of a method a conditional reply is for.
type Predicate = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// What a mock node answers to one method.
#[derive(Clone)]
pub(crate) enum NodeReply {
    Result(Value),
    Error {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

/// A JSON-RPC node that answers each method with what it was given, and
/// anything else as a node answers a method it does not have (`-32601`). It
/// keeps the methods it was asked, in order, so that a test can say a call was
/// never made.
#[derive(Clone)]
pub(crate) struct MockNode {
    replies: HashMap<String, NodeReply>,
    /// Replies that change from one ask to the next: the n-th ask gets the n-th
    /// value, and the last is repeated.
    sequences: HashMap<String, Vec<Value>>,
    /// `eth_call` replies by the selector of the call's data (`0x` and eight
    /// hex digits).
    calls: HashMap<String, NodeReply>,
    /// Replies for the asks whose parameters a test's predicate accepts; the
    /// first that does answers, before anything else.
    when: Vec<(String, Predicate, NodeReply)>,
    asked: Arc<Mutex<Vec<(String, Value)>>>,
}

impl MockNode {
    pub(crate) fn new() -> Self {
        Self {
            replies: HashMap::new(),
            sequences: HashMap::new(),
            calls: HashMap::new(),
            when: Vec::new(),
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A node on `chain_id` that reports a client version, has a nonce, a base
    /// fee, a priority fee and an estimate, and accepts a broadcast: what a
    /// signing sender needs.
    pub(crate) fn chain(chain_id: u64) -> Self {
        Self::new()
            .result("eth_chainId", json!(format!("0x{chain_id:x}")))
            .result("web3_clientVersion", json!("reth/v1.1.0-mock"))
            .result("eth_getTransactionCount", json!("0x0"))
            .result(
                "eth_getBlockByNumber",
                json!({"number": "0x64", "baseFeePerGas": "0x3b9aca00"}),
            )
            .result("eth_maxPriorityFeePerGas", json!("0x59682f00"))
            .result("eth_estimateGas", json!("0x5208"))
            .result("eth_blockNumber", json!("0x64"))
            .result(
                "eth_sendRawTransaction",
                json!(format!("0x{}", "ab".repeat(32))),
            )
    }

    pub(crate) fn result(mut self, method: &str, value: Value) -> Self {
        self.replies
            .insert(method.to_string(), NodeReply::Result(value));
        self
    }

    pub(crate) fn error(
        mut self,
        method: &str,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Self {
        self.replies.insert(
            method.to_string(),
            NodeReply::Error {
                code,
                message: message.to_string(),
                data,
            },
        );
        self
    }

    /// `method` is answered with each value in turn.
    pub(crate) fn sequence(mut self, method: &str, values: Vec<Value>) -> Self {
        self.sequences.insert(method.to_string(), values);
        self
    }

    /// An `eth_call` whose data starts with `selector` (`0x` and eight hex
    /// digits) is answered with `value`.
    pub(crate) fn eth_call(mut self, selector: &str, value: Value) -> Self {
        self.calls
            .insert(selector.to_string(), NodeReply::Result(value));
        self
    }

    /// `method` asked with parameters `predicate` accepts is answered with an
    /// error object, whatever else is set for it.
    pub(crate) fn error_when(
        mut self,
        method: &str,
        predicate: impl Fn(&Value) -> bool + Send + Sync + 'static,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Self {
        // The latest rule is the first asked, so a test can override a preset.
        self.when.insert(
            0,
            (
                method.to_string(),
                Arc::new(predicate),
                NodeReply::Error {
                    code,
                    message: message.to_string(),
                    data,
                },
            ),
        );
        self
    }

    /// The methods asked so far, with their parameters.
    pub(crate) fn asked(&self) -> Vec<(String, Value)> {
        self.asked.lock().unwrap().clone()
    }

    /// How many times `method` was asked.
    pub(crate) fn times_asked(&self, method: &str) -> usize {
        self.asked()
            .iter()
            .filter(|(asked, _)| asked == method)
            .count()
    }

    pub(crate) async fn start(&self) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(self.clone())
            .mount(&server)
            .await;
        server
    }
}

impl Respond for MockNode {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = body["method"].as_str().unwrap_or_default().to_string();
        self.asked
            .lock()
            .unwrap()
            .push((method.clone(), body["params"].clone()));
        let params = body["params"].clone();
        let nth = self
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|(asked, _)| *asked == method)
            .count();
        let sequenced = self
            .sequences
            .get(&method)
            .map(|values| NodeReply::Result(values[(nth - 1).min(values.len() - 1)].clone()));
        let by_selector = (method == "eth_call")
            .then(|| {
                params[0]["data"]
                    .as_str()
                    .map(|data| data.get(..10).unwrap_or(data))
            })
            .flatten()
            .and_then(|selector| self.calls.get(selector).cloned());
        let conditional = self
            .when
            .iter()
            .find(|(asked, predicate, _)| *asked == method && predicate(&params))
            .map(|(_, _, reply)| reply.clone());
        let reply = conditional
            .or(by_selector)
            .or(sequenced)
            .or_else(|| self.replies.get(&method).cloned());
        let envelope = match &reply {
            Some(NodeReply::Result(value)) => {
                json!({"jsonrpc": "2.0", "id": body["id"], "result": value})
            }
            Some(NodeReply::Error {
                code,
                message,
                data,
            }) => {
                let mut error = json!({"code": code, "message": message});
                if let Some(data) = data {
                    error["data"] = data.clone();
                }
                json!({"jsonrpc": "2.0", "id": body["id"], "error": error})
            }
            None => json!({"jsonrpc": "2.0", "id": body["id"], "error": {
                "code": -32601,
                "message": format!("the method {method} does not exist/is not available")
            }}),
        };
        ResponseTemplate::new(200).set_body_json(envelope)
    }
}
