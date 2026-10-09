//! The production test tier (`specs/V7-production-validation.md`).
//!
//! Everything here is `#[cfg(test)]`: the signed Binance client is
//! `pub(crate)`, so a test inside the crate is the only code that can drive it
//! the way a production run needs. Every test that touches a production venue
//! is `#[ignore]`d and run by a person, with keys and money:
//!
//! ```text
//! cargo test --lib production_lv1_binance -- --ignored --nocapture
//! ```
//!
//! **Nothing in this crate's build runs one.** What the build runs is each
//! case's self-test, against a mock standing in for the venue.
//!
//! The harness:
//!
//! | Part | Where | Rule |
//! |---|---|---|
//! | Host guard | [`guard`] | Binance: exactly `https://api.binance.com`; a node is not local and not an anvil |
//! | Spend ledger | [`ledger`] | `VP_ORDER_CAP_USD` per order, `VP_SPEND_CAP_USD` per run, checked before an order is signed |
//! | Halt file | [`gate`] | `VP_HALT_FILE`: present means stop, before every call |
//! | Dry run | [`gate`] | `VP_DRY_RUN=1`: print every call, send none |
//! | Record mode | [`record`] | `VP_RECORD_DIR`: each reply as the venue sent it, the `uid` redacted |
//! | Results | [`results`] | `VP_OUT`: `results.jsonl`, one line per call |
//! | Request hooks | `cex::binance::client::hooks` | a signed request with a chosen timestamp, no signature, a damaged one |
//!
//! A [`Run`] holds them together, and [`Run::call`] is how every call is
//! made: it times the call, reads its outcome, judges it against what the case
//! expects, and writes the line.

pub(crate) mod chain_checks;
pub(crate) mod env;
pub(crate) mod exchange_checks;
pub(crate) mod gate;
pub(crate) mod guard;
pub(crate) mod ledger;
pub(crate) mod lv1_base;
pub(crate) mod lv1_binance;
pub(crate) mod lv2a_exchange;
pub(crate) mod lv2b_chain;
pub(crate) mod record;
pub(crate) mod results;
pub(crate) mod sizing;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod wire;

use crate::cex::binance::{BinanceConfig, BinanceRest};
use crate::cex::CexTimings;
use crate::production::env::{flag, positive_decimal, required, Env};
use crate::production::gate::{Echo, Gate, OrderPermit};
use crate::production::ledger::{Ledger, Stop};
use crate::production::record::Recorder;
use crate::production::results::{
    judge, Expected, Kind, Outcome, Params, RequestRecord, ResultLine, Results, Tally, Verdict,
};
use crate::production::wire::{Call, Held, Wire};
use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The default for `VP_ORDER_CAP_USD`.
const DEFAULT_ORDER_CAP_USD: i64 = 12;

/// The variables that hold a key or a secret: their values are never written
/// to `results.jsonl`.
const SECRET_VARS: [&str; 7] = [
    "BINANCE_API_KEY",
    "BINANCE_API_SECRET",
    "BINANCE_RO_API_KEY",
    "BINANCE_RO_API_SECRET",
    "BINANCE_EMPTY_API_KEY",
    "BINANCE_EMPTY_API_SECRET",
    "VP_SIGNER_KEY_HEX",
];

/// One production run: its caps, its gate, its results.
pub(crate) struct Run {
    pub(crate) id: String,
    venue: String,
    host: String,
    gate: Arc<Gate>,
    ledger: Arc<Ledger>,
    results: Results,
    /// Text that must not reach a line as it is: a node URL carries its key in
    /// its path or query, and an error that names the URL repeats it.
    hidden: Mutex<Vec<String>>,
}

/// What a case says about a call it is about to make.
pub(crate) struct CallSpec<'a> {
    pub(crate) case: &'a str,
    pub(crate) expected: Expected,
    /// The request the line records, for a call that does not go through the
    /// wire (an EVM call). Otherwise the first call the gate saw.
    pub(crate) request: Option<RequestRecord>,
    /// Record mode: the call's replies are saved as `<step>-<name>`.
    pub(crate) record_as: Option<&'a str>,
    /// The host the line names, when it is not the run's (one run asks several
    /// providers).
    pub(crate) host: Option<String>,
}

impl<'a> CallSpec<'a> {
    pub(crate) fn new(case: &'a str, expected: Expected) -> Self {
        Self {
            case,
            expected,
            request: None,
            record_as: None,
            host: None,
        }
    }

    pub(crate) fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_string());
        self
    }

    pub(crate) fn record_as(mut self, stem: &'a str) -> Self {
        self.record_as = Some(stem);
        self
    }

    pub(crate) fn request(mut self, request: RequestRecord) -> Self {
        self.request = Some(request);
        self
    }
}

/// What a case makes of a call that answered, beyond its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grade {
    Pass,
    /// The answer is wrong. A `fail`.
    Fail(String),
    /// The answer cannot be judged, and the case says why (a trade that has left
    /// the window it is checked against). A `skipped`, never a pass.
    Skip(String),
}

impl From<std::result::Result<(), String>> for Grade {
    fn from(checked: std::result::Result<(), String>) -> Self {
        match checked {
            Ok(()) => Grade::Pass,
            Err(why) => Grade::Fail(why),
        }
    }
}

/// A call that was made, and what was made of it.
pub(crate) struct Called<T> {
    pub(crate) result: Result<T>,
    pub(crate) outcome: Outcome,
    pub(crate) verdict: Verdict,
    pub(crate) reason: String,
    /// The venue accepted a request the case expected it to refuse.
    accepted_a_refusal_case: bool,
}

impl<T> Called<T> {
    /// Whether the run must go no further: it was halted, or the venue accepted
    /// a request it must refuse (which may have spent money).
    pub(crate) fn stops_the_run(&self) -> bool {
        matches!(self.verdict, Verdict::Halted) || self.accepted_a_refusal_case
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Run {
    /// Reads the run's settings. `VP_SPEND_CAP_USD`, `VP_HALT_FILE` and `VP_OUT`
    /// are required and have no default; `VP_ORDER_CAP_USD` defaults to 12;
    /// `VP_DRY_RUN=1` and `VP_RECORD_DIR` are optional. A halt file that is
    /// already there is a run that does not start.
    pub(crate) fn start(env: &dyn Env, venue: &str, host: &str, echo: Echo) -> Result<Self> {
        let spend_cap = positive_decimal(env, "VP_SPEND_CAP_USD", None)?;
        let order_cap = positive_decimal(
            env,
            "VP_ORDER_CAP_USD",
            Some(Decimal::from(DEFAULT_ORDER_CAP_USD)),
        )?;
        let halt_file = PathBuf::from(required(env, "VP_HALT_FILE")?);
        let out = PathBuf::from(required(env, "VP_OUT")?);
        let dry_run = flag(env, "VP_DRY_RUN");
        if halt_file.exists() {
            bail!(
                "the halt file {} is present: remove it to start the run",
                halt_file.display()
            );
        }
        let recorder = env
            .var("VP_RECORD_DIR")
            .map(|dir| Recorder::create(&PathBuf::from(dir)))
            .transpose()?;
        let secrets = SECRET_VARS
            .iter()
            .filter_map(|var| env.var(var))
            .map(|value| value.trim_start_matches("0x").to_string())
            .collect();
        let results = Results::create(&out, secrets)?;
        Ok(Self {
            id: format!("run-{}-{}", now_ms(), std::process::id()),
            venue: venue.to_string(),
            host: host.to_string(),
            gate: Arc::new(Gate::new(halt_file, dry_run, echo, recorder)),
            ledger: Arc::new(Ledger::new(spend_cap, order_cap)),
            results,
            hidden: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    pub(crate) fn ledger(&self) -> &Arc<Ledger> {
        &self.ledger
    }

    /// Keeps `urls` (and the path and query of each, where its key is) out of
    /// every line: wherever the text appears it is replaced by `[provider]`.
    pub(crate) fn hide_urls(&self, urls: &[String]) {
        let mut hidden = self.hidden.lock().unwrap();
        for url in urls {
            hidden.push(url.clone());
            if let Ok(parsed) = reqwest::Url::parse(url) {
                for piece in [parsed.path(), parsed.query().unwrap_or_default()] {
                    if piece.trim_matches('/').len() >= 6 {
                        hidden.push(piece.to_string());
                    }
                }
            }
        }
        // The longest first, so a URL is replaced whole before its parts.
        hidden.sort_by_key(|text| std::cmp::Reverse(text.len()));
        hidden.dedup();
    }

    fn scrub(&self, text: &str) -> String {
        let hidden = self.hidden.lock().unwrap();
        hidden.iter().fold(text.to_string(), |text, secret| {
            text.replace(secret.as_str(), "[provider]")
        })
    }

    /// The wire a client is given, so that its calls go through the gate.
    pub(crate) fn wire(&self) -> Arc<dyn Wire> {
        self.gate.clone()
    }

    /// A signed Binance spot client for the keys `{prefix}_API_KEY` and
    /// `{prefix}_API_SECRET` (`BINANCE`, `BINANCE_RO`, `BINANCE_EMPTY`), at the
    /// host `BINANCE_BASE_URL` names, which must be `allowed_host` exactly.
    /// Its calls go through this run's gate.
    pub(crate) fn binance_rest(
        &self,
        env: &dyn Env,
        allowed_host: &str,
        prefix: &str,
        timings: CexTimings,
    ) -> Result<BinanceRest> {
        guard::binance_host(
            "BINANCE_BASE_URL",
            env.var("BINANCE_BASE_URL").as_deref(),
            allowed_host,
        )?;
        let config = BinanceConfig {
            base_url: allowed_host.to_string(),
            api_key: required(env, &format!("{prefix}_API_KEY"))?,
            api_secret: required(env, &format!("{prefix}_API_SECRET"))?,
        };
        Ok(BinanceRest::with_timings(config, timings).with_wire(self.wire()))
    }

    /// Whether the keys `{prefix}_API_KEY` and `{prefix}_API_SECRET` are set.
    pub(crate) fn has_keys(env: &dyn Env, prefix: &str) -> bool {
        env.var(&format!("{prefix}_API_KEY")).is_some()
            && env.var(&format!("{prefix}_API_SECRET")).is_some()
    }

    /// The ledger's say-so for an order of `quantity` at `price` (USD): its
    /// notional is checked against both caps now, before the order is signed,
    /// and the permit it returns lets the gate pass that one order.
    pub(crate) fn authorise_order(
        &self,
        quantity: Decimal,
        price_usd: Decimal,
    ) -> Result<OrderPermit<'_>, Stop> {
        let notional = quantity
            .checked_mul(price_usd)
            .ok_or_else(|| Stop::Uncomputable(format!("{quantity} at {price_usd} overflows")))?;
        self.ledger.authorise(notional)?;
        Ok(self.gate.permit_one_order())
    }

    /// Asks the gate whether a call that does not go through the wire (an EVM
    /// JSON-RPC call) may be made: halted and dry-run runs make none.
    pub(crate) fn permit_rpc(&self, method: &str, params: &[(&str, String)]) -> Result<()> {
        let call = Call {
            method: "RPC".to_string(),
            path: method.to_string(),
            params: params
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        };
        self.gate
            .permit(&call)
            .map_err(|held: Held| held.into_error())
    }

    /// Makes a call and writes its line. See [`Run::call_with`].
    pub(crate) async fn call<T, Fut>(
        &self,
        spec: CallSpec<'_>,
        make: impl FnOnce() -> Fut,
    ) -> Result<Called<T>>
    where
        Fut: Future<Output = Result<T>>,
    {
        self.call_with(spec, make, |_| None, |_| Ok(())).await
    }

    /// Makes a call, reads its outcome, judges it against `spec.expected` and
    /// writes its line. `references` is made from the call's result, after it:
    /// an object for a call that trades, `None` (written as `null`) for every
    /// other. `check` is what the case asserts of the result beyond its kind
    /// (a key that must not withdraw, a refusal that must not be a revert): a
    /// call whose outcome passed and whose check says no is a `fail`, with the
    /// check's reason.
    ///
    /// An `Err` is the harness failing (a line it may not write, a reply it
    /// could not record): the run stops. A call that failed is an `Ok` with a
    /// verdict.
    pub(crate) async fn call_with<T, Fut>(
        &self,
        spec: CallSpec<'_>,
        make: impl FnOnce() -> Fut,
        references: impl FnOnce(&Result<T>) -> Option<serde_json::Value>,
        check: impl FnOnce(&Result<T>) -> std::result::Result<(), String>,
    ) -> Result<Called<T>>
    where
        Fut: Future<Output = Result<T>>,
    {
        self.call_graded(spec, make, references, |result| check(result).into())
            .await
    }

    /// As [`Run::call_with`], with a [`Grade`] for a check that can also say it
    /// cannot judge the answer: the line is then `skipped`, with its reason.
    pub(crate) async fn call_graded<T, Fut>(
        &self,
        spec: CallSpec<'_>,
        make: impl FnOnce() -> Fut,
        references: impl FnOnce(&Result<T>) -> Option<serde_json::Value>,
        grade: impl FnOnce(&Result<T>) -> Grade,
    ) -> Result<Called<T>>
    where
        Fut: Future<Output = Result<T>>,
    {
        self.gate.begin_call();
        let started_ms = now_ms();
        let started = Instant::now();
        // The halt file is looked at before the call is made, for a call that
        // does not go through the gate as well as for one that does.
        let halted_before = self.gate.halted().map(Held::Halted);
        let result = if let Some(held) = halted_before.clone() {
            Err(held.into_error())
        } else {
            if let Some(stem) = spec.record_as {
                self.gate.record_as(stem);
            }
            make().await
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        self.gate.end_call();
        if let Some(why) = self.gate.recording_failure() {
            bail!("recording a reply failed: {why}");
        }

        let outcome = match &result {
            Ok(_) => Outcome::ok(),
            Err(err) => Outcome::of_error(err),
        };
        let held = halted_before.or_else(|| self.gate.take_held());
        let (mut verdict, mut reason) = judge(&spec.expected, &outcome, held.as_ref());
        if verdict == Verdict::Pass {
            match grade(&result) {
                Grade::Pass => {}
                Grade::Fail(why) => {
                    verdict = Verdict::Fail;
                    reason = why;
                }
                Grade::Skip(why) => {
                    verdict = Verdict::Skipped;
                    reason = why;
                }
            }
        }
        let accepted_a_refusal_case = spec.expected.kind == Kind::Refused
            && matches!(outcome, Outcome::Ok { .. })
            && held.is_none();
        let request = spec
            .request
            .or_else(|| self.first_request())
            .unwrap_or(RequestRecord {
                method: String::new(),
                path: String::new(),
                params: Params(Vec::new()),
            });
        let line = ResultLine {
            run_id: self.id.clone(),
            case: spec.case.to_string(),
            venue: self.venue.clone(),
            host: spec.host.clone().unwrap_or_else(|| self.host.clone()),
            started_ms,
            request,
            outcome: outcome.clone(),
            expected: spec.expected,
            verdict,
            reason: reason.clone(),
            body_file: spec.record_as.and_then(|_| self.gate.recorded_file()),
            latency_ms,
            references: references(&result),
        };
        self.results
            .write(&line.scrubbed(&|text| self.scrub(text)))?;
        Ok(Called {
            result,
            outcome,
            verdict,
            reason,
            accepted_a_refusal_case,
        })
    }

    /// Writes a line for a case that did not make a call: skipped for want of
    /// keys or inputs, or halted by the ledger.
    pub(crate) fn note(
        &self,
        case: &str,
        expected: Expected,
        verdict: Verdict,
        reason: &str,
    ) -> Result<()> {
        self.note_at(None, case, expected, verdict, reason)
    }

    /// As [`Run::note`], for a case run against `host` rather than the run's.
    pub(crate) fn note_at(
        &self,
        host: Option<&str>,
        case: &str,
        expected: Expected,
        verdict: Verdict,
        reason: &str,
    ) -> Result<()> {
        let line = ResultLine {
            run_id: self.id.clone(),
            case: case.to_string(),
            venue: self.venue.clone(),
            host: host.map_or_else(|| self.host.clone(), str::to_string),
            started_ms: now_ms(),
            request: RequestRecord {
                method: String::new(),
                path: String::new(),
                params: Params(Vec::new()),
            },
            outcome: Outcome::NotSent {
                msg: reason.to_string(),
            },
            expected,
            verdict,
            reason: reason.to_string(),
            body_file: None,
            latency_ms: 0,
            references: None,
        };
        self.results
            .write(&line.scrubbed(&|text| self.scrub(text)))
            .with_context(|| format!("writing the line for {case}"))
    }

    /// Writes a line for a check the case made by reading what it already has:
    /// no request, the venue's answer taken as read. `references` is an object
    /// where the check is of a call that traded and `None` otherwise.
    pub(crate) fn computed(
        &self,
        case: &str,
        verdict: Verdict,
        reason: &str,
        references: Option<serde_json::Value>,
    ) -> Result<()> {
        let line = ResultLine {
            run_id: self.id.clone(),
            case: case.to_string(),
            venue: self.venue.clone(),
            host: self.host.clone(),
            started_ms: now_ms(),
            request: RequestRecord {
                method: String::new(),
                path: String::new(),
                params: Params(Vec::new()),
            },
            outcome: Outcome::ok(),
            expected: Expected::ok(),
            verdict,
            reason: reason.to_string(),
            body_file: None,
            latency_ms: 0,
            references,
        };
        self.results
            .write(&line.scrubbed(&|text| self.scrub(text)))
            .with_context(|| format!("writing the line for {case}"))
    }

    /// How many calls ended each way so far.
    pub(crate) fn tally(&self) -> Tally {
        self.results.tally()
    }

    /// The run is over: `Ok` only if nothing failed, nothing was halted and
    /// nothing is unmapped.
    pub(crate) fn finish(&self) -> Result<Tally> {
        let tally = self.tally();
        if !tally.is_clean() {
            bail!(
                "the run is not clean: {} failed, {} halted, {} unmapped ({} passed, {} skipped); \
                 see results.jsonl",
                tally.fail,
                tally.halted,
                tally.unmapped,
                tally.pass,
                tally.skipped
            );
        }
        Ok(tally)
    }

    /// The first call the gate saw for the call being made, as a line records
    /// it. The venue's clock reads are not the call.
    fn first_request(&self) -> Option<RequestRecord> {
        self.gate
            .calls()
            .into_iter()
            .find(|call| !call.path.ends_with("/time"))
            .map(|call| RequestRecord {
                method: call.method,
                path: call.path,
                params: Params(call.params),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::SpotBalanceReader;
    use crate::cex::binance::BinanceLive;
    use crate::cex::{CexExecutor, OrderRequest, OrderSide};
    use crate::production::testing::{fast, Setup};
    use serde_json::json;
    use std::collections::HashMap;
    use std::str::FromStr;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    /// A venue with a clock, an account, a book and an order that fills.
    async fn venue() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"serverTime": crate::cex::binance::clock::local_now_ms()}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\n  \"updateTime\": 1,\n  \"balances\": [\n    {\"asset\": \"USDT\", \"free\": \"20.5\", \"locked\": \"0\"}\n  ],\n  \"uid\": 354937868\n}",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/bookTicker"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "symbol": "AEROUSDT", "bidPrice": "1.0000", "bidQty": "100",
                "askPrice": "1.0010", "askQty": "100"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "orderId": 42, "status": "FILLED", "executedQty": "10",
                "transactTime": 1_700_000_000_000u64,
                "fills": [{"price": "1.0010", "qty": "10", "commission": "0.01",
                           "commissionAsset": "USDT", "tradeId": 7}]
            })))
            .mount(&server)
            .await;
        server
    }

    fn live(rest: BinanceRest) -> BinanceLive {
        BinanceLive::new(rest, HashMap::from([("AEROUSDT".to_string(), d("0.1"))]))
    }

    fn market_buy(quantity: &str) -> OrderRequest {
        OrderRequest {
            symbol: "AEROUSDT".to_string(),
            side: OrderSide::Buy,
            quantity: d(quantity),
            quoted_price: d("1"),
            reduce_only: false,
        }
    }

    // --- start ---

    #[tokio::test]
    async fn a_run_needs_its_cap_its_halt_file_and_its_output_and_none_has_a_default() {
        let server = MockServer::start().await;
        for missing in ["VP_SPEND_CAP_USD", "VP_HALT_FILE", "VP_OUT"] {
            let setup = Setup::new("missing", &server).without(missing);
            let err = setup.run(&server).err().expect("refused").to_string();
            assert!(err.contains(missing), "{missing}: {err}");
            setup.clean_up();
        }
    }

    #[tokio::test]
    async fn the_order_cap_defaults_to_twelve_and_a_cap_that_is_not_positive_is_refused() {
        let server = MockServer::start().await;
        let setup = Setup::new("caps", &server);
        let run = setup.run(&server).unwrap();
        assert_eq!(run.ledger().order_cap(), d("12"));
        assert_eq!(run.ledger().spend_cap(), d("30"));
        setup.clean_up();

        let setup = Setup::new("zero", &server).with("VP_SPEND_CAP_USD", "0");
        assert!(setup.run(&server).is_err());
        setup.clean_up();
    }

    #[tokio::test]
    async fn a_halt_file_that_is_already_there_is_a_run_that_does_not_start() {
        let server = MockServer::start().await;
        let setup = Setup::new("halted-at-start", &server);
        std::fs::create_dir_all(setup.halt.parent().unwrap()).unwrap();
        std::fs::write(&setup.halt, "").unwrap();
        let err = setup.run(&server).err().expect("refused").to_string();
        assert!(err.contains("halt file"), "{err}");
        setup.clean_up();
    }

    // --- the host guard, through the run ---

    #[tokio::test]
    async fn the_run_builds_a_client_only_for_the_host_it_was_allowed() {
        let server = MockServer::start().await;
        let setup = Setup::new("guard", &server);
        let run = setup.run(&server).unwrap();

        // The environment names the mock and the mock is the allowed host: built.
        assert!(run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .is_ok());
        // The environment names the testnet: refused by name.
        let testnet = Setup::new("guard-testnet", &server)
            .with("BINANCE_BASE_URL", "https://testnet.binance.vision");
        let err = run
            .binance_rest(
                &testnet.env(),
                guard::PRODUCTION_BINANCE_SPOT,
                "BINANCE",
                fast(),
            )
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("Spot testnet"), "{err}");
        // The environment names nothing: refused by name.
        let unset = Setup::new("guard-unset", &server).without("BINANCE_BASE_URL");
        let err = run
            .binance_rest(
                &unset.env(),
                guard::PRODUCTION_BINANCE_SPOT,
                "BINANCE",
                fast(),
            )
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("BINANCE_BASE_URL is not set"), "{err}");
        // The mock is not production.
        let err = run
            .binance_rest(
                &setup.env(),
                guard::PRODUCTION_BINANCE_SPOT,
                "BINANCE",
                fast(),
            )
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("exactly https://api.binance.com"), "{err}");
        // No keys: refused by name.
        let keyless = Setup::new("guard-keys", &server).without("BINANCE_API_SECRET");
        let err = run
            .binance_rest(&keyless.env(), &server.uri(), "BINANCE", fast())
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("BINANCE_API_SECRET"), "{err}");
        assert!(Run::has_keys(&setup.env(), "BINANCE"));
        assert!(!Run::has_keys(&setup.env(), "BINANCE_RO"));
        for s in [&setup, &testnet, &unset, &keyless] {
            s.clean_up();
        }
    }

    // --- dry run ---

    /// "Dry run sends nothing, asserted by the mock receiving zero requests":
    /// not the order, not the signed read, not the public read, and not the
    /// clock read a signed call would make first.
    #[tokio::test]
    async fn a_dry_run_sends_nothing_and_every_call_is_skipped() {
        let server = venue().await;
        let setup = Setup::new("dry", &server).with("VP_DRY_RUN", "1");
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();
        let live = live(rest);

        let ticker = run
            .call(CallSpec::new("B10", Expected::ok()), || async {
                live.rest().book_ticker("AEROUSDT").await
            })
            .await
            .unwrap();
        let balances = run
            .call(CallSpec::new("B10", Expected::ok()), || async {
                live.balances().await
            })
            .await
            .unwrap();
        let permit = run.authorise_order(d("10"), d("1")).unwrap();
        let order = run
            .call(CallSpec::new("B7", Expected::refused(&[-2010])), || async {
                live.execute(&market_buy("10")).await
            })
            .await
            .unwrap();
        drop(permit);

        assert_eq!(server.received_requests().await.unwrap().len(), 0);
        for called in [&ticker.verdict, &balances.verdict, &order.verdict] {
            assert_eq!(*called, Verdict::Skipped);
        }
        assert_eq!(ticker.outcome.kind(), Kind::NotSent);
        assert!(ticker.reason.contains("dry run"), "{}", ticker.reason);
        let printed = run.gate().printed();
        assert_eq!(printed.len(), 3, "{printed:?}");
        assert!(printed[0].starts_with("DRY RUN GET /api/v3/ticker/bookTicker symbol=AEROUSDT"));
        assert!(printed[2].contains("POST /api/v3/order") && printed[2].contains("side=BUY"));
        // No key, no signature, no timestamp in anything printed or written.
        let written = std::fs::read_to_string(setup.out.join("results.jsonl")).unwrap();
        for text in printed.iter().map(String::as_str).chain([written.as_str()]) {
            for forbidden in ["AKEY-1234", "SECRET-9876", "signature", "timestamp"] {
                assert!(!text.contains(forbidden), "{forbidden} in {text}");
            }
        }
        let lines = setup.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2]["request"]["path"], "/api/v3/order");
        assert_eq!(lines[2]["request"]["params"]["symbol"], "AEROUSDT");
        assert_eq!(lines[2]["verdict"], "skipped");
        assert!(run.finish().is_ok(), "a dry run is clean: nothing failed");
        setup.clean_up();
    }

    /// The venue's clock is a request like any other: a dry run does not read it
    /// and a halted run does not either.
    #[tokio::test]
    async fn the_venues_clock_is_not_read_in_a_dry_run_or_after_the_halt_file() {
        let server = venue().await;
        let setup = Setup::new("clock", &server).with("VP_DRY_RUN", "1");
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();
        let err = rest.client().sync_clock().await.unwrap_err();
        assert!(format!("{err:#}").contains("dry run"), "{err:#}");
        assert_eq!(run.gate().printed(), ["DRY RUN GET /api/v3/time"]);
        setup.clean_up();

        let setup = Setup::new("clock-halt", &server);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();
        std::fs::write(&setup.halt, "").unwrap();
        let err = rest.client().sync_clock().await.unwrap_err();
        assert!(format!("{err:#}").contains("halted"), "{err:#}");
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
        setup.clean_up();
    }

    /// An EVM call does not go through the Binance client, so the case asks the
    /// gate itself: a dry run prints it and makes none, and the halt file stops it.
    #[tokio::test]
    async fn a_call_outside_the_wire_is_held_back_by_a_dry_run_and_by_the_halt_file() {
        let server = venue().await;
        let rpc_request = || RequestRecord {
            method: "RPC".to_string(),
            path: "eth_sendRawTransaction".to_string(),
            params: Params(vec![("chain".to_string(), "8453".to_string())]),
        };

        let setup = Setup::new("rpc-dry", &server).with("VP_DRY_RUN", "1");
        let run = setup.run(&server).unwrap();
        let made = std::sync::atomic::AtomicBool::new(false);
        let called = run
            .call(
                CallSpec::new("C1", Expected::refused(&[]))
                    .request(rpc_request())
                    .record_as("01-send"),
                || async {
                    run.permit_rpc("eth_sendRawTransaction", &[("chain", "8453".to_string())])?;
                    made.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(!made.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(called.verdict, Verdict::Skipped);
        assert_eq!(
            run.gate().printed(),
            ["DRY RUN RPC eth_sendRawTransaction chain=8453"]
        );
        assert_eq!(
            setup.lines()[0]["request"]["path"],
            "eth_sendRawTransaction"
        );
        assert_eq!(setup.lines()[0]["body_file"], serde_json::Value::Null);
        setup.clean_up();

        let setup = Setup::new("rpc-halt", &server);
        let run = setup.run(&server).unwrap();
        std::fs::write(&setup.halt, "").unwrap();
        let called = run
            .call(
                CallSpec::new("C1", Expected::ok()).request(rpc_request()),
                || async {
                    made.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(
            !made.load(std::sync::atomic::Ordering::SeqCst),
            "never made"
        );
        assert_eq!(called.verdict, Verdict::Halted);
        setup.clean_up();
    }

    // --- halt file ---

    #[tokio::test]
    async fn the_halt_file_stops_the_next_call_and_it_is_written_as_halted() {
        let server = venue().await;
        let setup = Setup::new("halt", &server);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();

        let before = run
            .call(CallSpec::new("B10", Expected::ok()), || async {
                rest.book_ticker("AEROUSDT").await
            })
            .await
            .unwrap();
        assert_eq!(before.verdict, Verdict::Pass);
        assert!(!before.stops_the_run());

        std::fs::write(&setup.halt, "stop").unwrap();
        let sent_before = server.received_requests().await.unwrap().len();
        let after = run
            .call(CallSpec::new("B10", Expected::ok()), || async {
                rest.book_ticker("AEROUSDT").await
            })
            .await
            .unwrap();

        assert_eq!(after.verdict, Verdict::Halted);
        assert!(after.stops_the_run());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            sent_before,
            "nothing was sent after the halt file appeared"
        );
        assert_eq!(setup.lines()[1]["verdict"], "halted");
        assert!(run.finish().is_err(), "a halted run is not a clean one");
        setup.clean_up();
    }

    // --- ledger and the order gate ---

    #[tokio::test]
    async fn an_order_is_placed_only_with_the_ledgers_say_so_and_only_once() {
        let server = venue().await;
        let setup = Setup::new("ledger", &server);
        let run = setup.run(&server).unwrap();
        let live = live(
            run.binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
                .unwrap(),
        );
        let orders_sent = || async {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/api/v3/order")
                .count()
        };

        // Without the ledger's say-so the order is not signed: not even the
        // clock is read for it.
        let unchecked = run
            .call(CallSpec::new("B7", Expected::refused(&[-2010])), || async {
                live.execute(&market_buy("10")).await
            })
            .await
            .unwrap();
        assert_eq!(unchecked.verdict, Verdict::Halted);
        assert!(
            unchecked.reason.contains("no ledger check"),
            "{}",
            unchecked.reason
        );
        assert_eq!(orders_sent().await, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 0);

        // An order over the order cap is refused by the ledger before anything.
        assert_eq!(
            run.authorise_order(d("13"), d("1")).err(),
            Some(Stop::OrderOverCap {
                notional: d("13"),
                cap: d("12")
            })
        );
        assert_eq!(orders_sent().await, 0);

        // With it, one order goes, and the permit is spent.
        let permit = run.authorise_order(d("10"), d("1")).unwrap();
        let filled = run
            .call(CallSpec::new("LV2a", Expected::ok()), || async {
                live.execute(&market_buy("10")).await
            })
            .await
            .unwrap();
        assert_eq!(filled.verdict, Verdict::Pass);
        assert_eq!(filled.result.unwrap().filled_qty, d("10"));
        assert_eq!(orders_sent().await, 1);
        let again = run
            .call(CallSpec::new("LV2a", Expected::ok()), || async {
                live.execute(&market_buy("10")).await
            })
            .await
            .unwrap();
        assert_eq!(
            again.verdict,
            Verdict::Halted,
            "the second order has no say-so"
        );
        assert_eq!(orders_sent().await, 1);
        drop(permit);
        setup.clean_up();
    }

    #[tokio::test]
    async fn the_ledger_refuses_the_order_that_would_pass_the_cap() {
        let server = MockServer::start().await;
        let setup = Setup::new("cap", &server).with("VP_SPEND_CAP_USD", "15");
        let run = setup.run(&server).unwrap();
        run.ledger().record_loss(d("4")).unwrap();

        // 4 lost and 12 at stake could pass 15.
        let err = run.authorise_order(d("12"), d("1")).err().expect("refused");
        assert_eq!(
            err,
            Stop::WouldPassSpendCap {
                loss: d("4"),
                notional: d("12"),
                cap: d("15")
            }
        );
        assert!(run.authorise_order(d("11"), d("1")).is_ok());
        // A notional that cannot be computed is no say-so.
        assert!(matches!(
            run.authorise_order(Decimal::MAX, d("2")).err(),
            Some(Stop::Uncomputable(_))
        ));
        setup.clean_up();
    }

    // --- record mode ---

    #[tokio::test]
    async fn record_mode_saves_the_reply_with_only_the_uid_changed_and_names_it_in_the_line() {
        let server = venue().await;
        let setup = Setup::new("record", &server);
        let record = setup.record.display().to_string();
        let setup = setup.with("VP_RECORD_DIR", &record);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();

        let called = run
            .call(
                CallSpec::new("B10", Expected::ok()).record_as("01-account"),
                || async { rest.balances().await },
            )
            .await
            .unwrap();

        assert_eq!(called.verdict, Verdict::Pass);
        let saved = std::fs::read_to_string(setup.record.join("01-account.json")).unwrap();
        let sent = "{\n  \"updateTime\": 1,\n  \"balances\": [\n    {\"asset\": \"USDT\", \"free\": \"20.5\", \"locked\": \"0\"}\n  ],\n  \"uid\": 354937868\n}";
        assert_eq!(saved.replace("\"REDACTED-UID\"", "354937868"), sent);
        assert!(!saved.contains("354937868"));
        assert_eq!(
            std::fs::read_to_string(setup.record.join("01-account.status")).unwrap(),
            "200\n"
        );
        assert_eq!(
            std::fs::read_dir(&setup.record).unwrap().count(),
            2,
            "the clock reply is not saved"
        );
        assert_eq!(setup.lines()[0]["body_file"], "01-account.json");
        setup.clean_up();
    }

    /// A case asserts more than a kind: a key that must not withdraw, a refusal
    /// that must not be a revert. A call that answered and fails the assertion
    /// fails, with the assertion's own words.
    #[tokio::test]
    async fn a_check_that_says_no_turns_a_pass_into_a_fail_with_its_reason() {
        let server = venue().await;
        let setup = Setup::new("check", &server);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();

        let called = run
            .call_with(
                CallSpec::new("B10", Expected::ok()),
                || async { rest.book_ticker("AEROUSDT").await },
                |_| None,
                |result| match result {
                    Ok(ticker) if ticker.bid_price > d("5") => Ok(()),
                    _ => Err("the bid is not above 5".to_string()),
                },
            )
            .await
            .unwrap();

        assert_eq!(called.verdict, Verdict::Fail);
        assert_eq!(called.reason, "the bid is not above 5");
        assert!(
            !called.stops_the_run(),
            "a failed assertion does not stop a read-only run"
        );
        assert_eq!(setup.lines()[0]["verdict"], "fail");
        assert!(run.finish().is_err());
        setup.clean_up();
    }

    /// "A refused case that is accepted is a fail and the run stops at once."
    #[tokio::test]
    async fn a_request_the_venue_was_to_refuse_and_accepted_stops_the_run() {
        let server = venue().await;
        let setup = Setup::new("accepted", &server);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();

        let called = run
            .call(CallSpec::new("B8", Expected::refused(&[-1121])), || async {
                rest.book_ticker("AEROUSDT").await
            })
            .await
            .unwrap();

        assert_eq!(called.verdict, Verdict::Fail);
        assert!(called.reason.contains("accepted"), "{}", called.reason);
        assert!(called.stops_the_run());
        setup.clean_up();
    }

    // --- results ---

    #[tokio::test]
    async fn a_key_a_secret_or_a_uid_that_reaches_a_line_stops_the_run() {
        let server = venue().await;
        let setup = Setup::new("leak", &server);
        let run = setup.run(&server).unwrap();

        let err = run
            .call(CallSpec::new("B1", Expected::ok()), || async {
                Err::<(), _>(anyhow::anyhow!(
                    "the venue said AKEY-1234-ABCD is not allowed"
                ))
            })
            .await
            .err()
            .expect("the harness stops")
            .to_string();
        assert!(err.contains("a key or a secret"), "{err}");

        let err = run
            .note("B2", Expected::ok(), Verdict::Skipped, "SECRET-9876-WXYZ")
            .unwrap_err();
        assert!(format!("{err:#}").contains("a key or a secret"), "{err:#}");
        assert_eq!(setup.lines().len(), 0, "neither line was written");
        setup.clean_up();
    }

    #[tokio::test]
    async fn references_are_written_as_an_object_or_as_null() {
        let server = venue().await;
        let setup = Setup::new("refs", &server);
        let run = setup.run(&server).unwrap();
        let rest = run
            .binance_rest(&setup.env(), &server.uri(), "BINANCE", fast())
            .unwrap();

        run.call(CallSpec::new("B10", Expected::ok()), || async {
            rest.book_ticker("AEROUSDT").await
        })
        .await
        .unwrap();
        run.call_with(
            CallSpec::new("LV2a", Expected::ok()),
            || async { rest.book_ticker("AEROUSDT").await },
            |result| {
                result
                    .as_ref()
                    .ok()
                    .map(|ticker| json!({"touch_at_send": {"bid": ticker.bid_price.to_string()}}))
            },
            |_| Ok(()),
        )
        .await
        .unwrap();

        let lines = setup.lines();
        assert!(lines[0]["references"].is_null());
        assert_eq!(lines[1]["references"]["touch_at_send"]["bid"], "1.0000");
        setup.clean_up();
    }

    #[tokio::test]
    async fn a_case_without_keys_or_inputs_is_skipped_with_its_reason_and_the_run_stays_clean() {
        let server = MockServer::start().await;
        let setup = Setup::new("skip", &server);
        let run = setup.run(&server).unwrap();

        run.note(
            "B5",
            Expected::refused(&[-2015]),
            Verdict::Skipped,
            "BINANCE_RO_API_KEY is not set",
        )
        .unwrap();

        let lines = setup.lines();
        assert_eq!(lines[0]["verdict"], "skipped");
        assert_eq!(lines[0]["reason"], "BINANCE_RO_API_KEY is not set");
        assert!(run.finish().is_ok());
        setup.clean_up();
    }
}
