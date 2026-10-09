//! The gate every production call goes through: the halt file, the dry run,
//! the rule that no order is signed without the ledger's say, and record mode.
//!
//! It is the [`Wire`] the Binance client consults. A call it holds back is
//! never sent; the client turns that into `ApiError::NotSent`, and the gate
//! remembers why ([`Gate::take_held`]) so that the run can write the right
//! verdict: `skipped` for a dry run, `halted` for the halt file.

use crate::production::record::Recorder;
use crate::production::wire::{Call, Held, Wire};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the dry run's lines go.
pub(crate) enum Echo {
    /// Printed to the standard output, as an ignored test run by hand shows it.
    Stdout,
    /// Kept only, for a test to read.
    Quiet,
}

pub(crate) struct Gate {
    halt_file: PathBuf,
    dry_run: bool,
    echo: Echo,
    recorder: Option<Recorder>,
    /// Set by the ledger's say-so for one order; taken by that order.
    armed: AtomicBool,
    state: Mutex<State>,
}

/// One request the client sent, as the gate saw it leave and come back. Kept
/// in memory for the case that made the call; never written to a line as it is
/// (the body can hold an account's `uid`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Exchange {
    pub(crate) method: String,
    pub(crate) path: String,
    /// The local clock, in ns since the epoch, when the request was about to
    /// leave: signed, built, and not yet handed to the connection.
    pub(crate) sent_ns: u128,
    /// The local clock when the reply's body had been read. `None` for a
    /// request that got no readable reply.
    pub(crate) returned_ns: Option<u128>,
    pub(crate) status: Option<u16>,
    /// The reply's body exactly as the venue sent it.
    pub(crate) body: Option<String>,
}

/// The local clock in ns since the epoch.
pub(crate) fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

#[derive(Default)]
struct State {
    /// What a dry run printed.
    printed: Vec<String>,
    /// The calls asked for since the last [`Gate::begin_call`], in order.
    calls: Vec<Call>,
    /// The requests sent since the last [`Gate::begin_call`], in order.
    exchanges: Vec<Exchange>,
    /// Why the last call that was held back was.
    held: Option<Held>,
}

/// The ledger said yes to one order. Dropping this takes it back, so an order
/// that was never sent cannot leave the permission for the next.
pub(crate) struct OrderPermit<'a> {
    gate: &'a Gate,
}

impl Drop for OrderPermit<'_> {
    fn drop(&mut self) {
        self.gate.armed.store(false, Ordering::SeqCst);
    }
}

impl Gate {
    pub(crate) fn new(
        halt_file: PathBuf,
        dry_run: bool,
        echo: Echo,
        recorder: Option<Recorder>,
    ) -> Self {
        Self {
            halt_file,
            dry_run,
            echo,
            recorder,
            armed: AtomicBool::new(false),
            state: Mutex::new(State::default()),
        }
    }

    /// The halt file's path if it is there. Checked before every call, by the
    /// gate and by whoever makes a call that does not go through it.
    pub(crate) fn halted(&self) -> Option<String> {
        self.halt_file
            .exists()
            .then(|| self.halt_file.display().to_string())
    }

    /// The ledger has said yes to one order: the gate lets the next order
    /// placement through, once.
    pub(crate) fn permit_one_order(&self) -> OrderPermit<'_> {
        self.armed.store(true, Ordering::SeqCst);
        OrderPermit { gate: self }
    }

    /// A new call begins: forget the last one's calls and reason.
    pub(crate) fn begin_call(&self) {
        let mut state = self.state.lock().unwrap();
        state.calls.clear();
        state.exchanges.clear();
        state.held = None;
    }

    /// The requests sent since [`Gate::begin_call`], with when each left and
    /// came back and what the venue answered. A call that is retried (a `-1021`
    /// read again) appears once for each request.
    pub(crate) fn exchanges(&self) -> Vec<Exchange> {
        self.state.lock().unwrap().exchanges.clone()
    }

    /// The calls asked for since [`Gate::begin_call`], held back or not.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Why a call since the last [`Gate::begin_call`] was held back, if one was.
    pub(crate) fn take_held(&self) -> Option<Held> {
        self.state.lock().unwrap().held.take()
    }

    /// What a dry run has printed.
    pub(crate) fn printed(&self) -> Vec<String> {
        self.state.lock().unwrap().printed.clone()
    }

    /// The next call's replies are saved under `stem` (record mode).
    pub(crate) fn record_as(&self, stem: &str) {
        if let Some(recorder) = &self.recorder {
            recorder.name_next(stem);
        }
    }

    /// The call is over (record mode).
    pub(crate) fn end_call(&self) {
        if let Some(recorder) = &self.recorder {
            recorder.end_call();
        }
    }

    /// The file the call's first reply was saved to.
    pub(crate) fn recorded_file(&self) -> Option<String> {
        self.recorder.as_ref().and_then(Recorder::first_file)
    }

    /// What went wrong with recording, if anything did.
    pub(crate) fn recording_failure(&self) -> Option<String> {
        self.recorder.as_ref().and_then(Recorder::failure)
    }

    fn decide(&self, call: &Call) -> Result<(), Held> {
        if let Some(why) = self.recording_failure() {
            return Err(Held::Failed(why));
        }
        if let Some(file) = self.halted() {
            return Err(Held::Halted(file));
        }
        if self.dry_run {
            let line = format!("DRY RUN {call}");
            if matches!(self.echo, Echo::Stdout) {
                println!("{line}");
            }
            self.state.lock().unwrap().printed.push(line);
            return Err(Held::DryRun);
        }
        if call.places_an_order() && !self.armed.swap(false, Ordering::SeqCst) {
            return Err(Held::Unauthorised(format!(
                "{call} was asked for with no ledger check before it"
            )));
        }
        Ok(())
    }
}

impl Wire for Gate {
    fn permit(&self, call: &Call) -> Result<(), Held> {
        let decision = self.decide(call);
        let mut state = self.state.lock().unwrap();
        state.calls.push(call.clone());
        if let Err(held) = &decision {
            state.held = Some(held.clone());
        }
        decision
    }

    fn sending(&self, method: &str, path: &str) {
        self.state.lock().unwrap().exchanges.push(Exchange {
            method: method.to_string(),
            path: path.to_string(),
            sent_ns: now_ns(),
            returned_ns: None,
            status: None,
            body: None,
        });
    }

    fn observed(&self, method: &str, path: &str, status: u16, body: &str) {
        let returned_ns = now_ns();
        if let Some(open) = self
            .state
            .lock()
            .unwrap()
            .exchanges
            .iter_mut()
            .rev()
            .find(|e| e.returned_ns.is_none() && e.method == method && e.path == path)
        {
            open.returned_ns = Some(returned_ns);
            open.status = Some(status);
            open.body = Some(body.to_string());
        }
        if let Some(recorder) = &self.recorder {
            recorder.observe(path, status, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Method;

    fn call(method: Method, path: &str, params: &[(&str, &str)]) -> Call {
        let params: Vec<(&str, String)> = params
            .iter()
            .map(|(key, value)| (*key, (*value).to_string()))
            .collect();
        Call::new(&method, path, &params)
    }

    fn gate(dry_run: bool) -> Gate {
        Gate::new(
            std::env::temp_dir().join(format!("venue-ports-no-such-halt-{}", std::process::id())),
            dry_run,
            Echo::Quiet,
            None,
        )
    }

    fn order() -> Call {
        call(
            Method::POST,
            "/api/v3/order",
            &[("symbol", "AEROUSDT"), ("side", "BUY")],
        )
    }

    #[test]
    fn a_dry_run_prints_the_call_and_holds_it_back() {
        let gate = gate(true);
        assert_eq!(gate.permit(&order()), Err(Held::DryRun));
        assert_eq!(
            gate.printed(),
            ["DRY RUN POST /api/v3/order symbol=AEROUSDT side=BUY"]
        );
        assert_eq!(gate.take_held(), Some(Held::DryRun));
        assert_eq!(gate.take_held(), None, "taken once");
        assert_eq!(gate.calls().len(), 1);
    }

    /// An ignored test run by hand sees the dry run on its standard output;
    /// the line is kept as well.
    #[test]
    fn a_dry_run_can_echo_to_the_standard_output_and_still_keeps_the_line() {
        let gate = Gate::new(
            std::env::temp_dir().join(format!("venue-ports-no-such-halt-{}", std::process::id())),
            true,
            Echo::Stdout,
            None,
        );
        assert_eq!(gate.permit(&order()), Err(Held::DryRun));
        assert_eq!(gate.printed().len(), 1);
    }

    #[test]
    fn a_dry_run_asks_for_no_ledger_check_because_it_places_nothing() {
        let gate = gate(true);
        assert_eq!(gate.permit(&order()), Err(Held::DryRun));
    }

    #[test]
    fn an_order_with_no_ledger_check_before_it_is_not_signed() {
        let gate = gate(false);
        assert!(matches!(gate.permit(&order()), Err(Held::Unauthorised(_))));
        assert!(matches!(gate.take_held(), Some(Held::Unauthorised(_))));
        // Reads and a test order need none.
        assert_eq!(
            gate.permit(&call(Method::GET, "/api/v3/account", &[])),
            Ok(())
        );
        assert_eq!(
            gate.permit(&call(Method::POST, "/api/v3/order/test", &[])),
            Ok(())
        );
    }

    #[test]
    fn the_ledgers_yes_is_for_one_order_and_ends_with_its_permit() {
        let gate = gate(false);
        {
            let _permit = gate.permit_one_order();
            assert_eq!(gate.permit(&order()), Ok(()));
            assert!(
                matches!(gate.permit(&order()), Err(Held::Unauthorised(_))),
                "the second order has no say-so of its own"
            );
        }
        // A permit that was never used is taken back when it is dropped.
        drop(gate.permit_one_order());
        assert!(matches!(gate.permit(&order()), Err(Held::Unauthorised(_))));
    }

    #[test]
    fn the_halt_file_stops_every_call_and_is_looked_at_each_time() {
        let file = std::env::temp_dir().join(format!("venue-ports-halt-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let gate = Gate::new(file.clone(), false, Echo::Quiet, None);
        let read = call(Method::GET, "/api/v3/account", &[]);

        assert_eq!(gate.permit(&read), Ok(()));
        std::fs::write(&file, "").unwrap();
        assert!(matches!(gate.permit(&read), Err(Held::Halted(_))));
        assert!(
            matches!(gate.take_held(), Some(Held::Halted(path)) if path.contains("venue-ports-halt"))
        );
        assert!(gate.halted().is_some());
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            gate.permit(&read),
            Ok(()),
            "removing the file lets the run go on"
        );
    }

    #[test]
    fn the_halt_file_wins_over_a_dry_run_and_prints_nothing() {
        let file = std::env::temp_dir().join(format!("venue-ports-halt2-{}", std::process::id()));
        std::fs::write(&file, "").unwrap();
        let gate = Gate::new(file.clone(), true, Echo::Quiet, None);
        assert!(matches!(gate.permit(&order()), Err(Held::Halted(_))));
        assert!(gate.printed().is_empty());
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn a_request_is_stamped_as_it_leaves_and_as_its_reply_is_read() {
        let gate = gate(false);
        let before = now_ns();
        gate.sending("POST", "/api/v3/order");
        gate.observed("POST", "/api/v3/order", 200, "{\"orderId\":1}");
        let after = now_ns();

        let exchanges = gate.exchanges();
        assert_eq!(exchanges.len(), 1);
        let sent = &exchanges[0];
        assert_eq!(
            (sent.status, sent.body.as_deref()),
            (Some(200), Some("{\"orderId\":1}"))
        );
        let (left, back) = (sent.sent_ns, sent.returned_ns.unwrap());
        assert!(
            before <= left && left <= back && back <= after,
            "{before} {left} {back} {after}"
        );
    }

    /// A reply belongs to the request of its own path that has none yet, the
    /// latest first: a retried request is two exchanges.
    #[test]
    fn a_reply_is_matched_to_its_own_request_and_a_retry_is_two_exchanges() {
        let gate = gate(false);
        gate.sending("GET", "/api/v3/time");
        gate.observed("GET", "/api/v3/time", 200, "{}");
        gate.sending("POST", "/api/v3/order");
        gate.observed("POST", "/api/v3/order", 400, "{\"code\":-1021}");
        gate.sending("POST", "/api/v3/order");
        gate.observed("POST", "/api/v3/order", 200, "{\"orderId\":2}");

        let exchanges = gate.exchanges();
        let statuses: Vec<_> = exchanges.iter().map(|e| e.status).collect();
        assert_eq!(statuses, [Some(200), Some(400), Some(200)]);
        assert_eq!(exchanges[2].body.as_deref(), Some("{\"orderId\":2}"));
    }

    /// A request with no readable reply has a departure and no return.
    #[test]
    fn a_request_with_no_reply_has_no_return_and_a_new_call_forgets_it() {
        let gate = gate(false);
        gate.sending("POST", "/api/v3/order");
        let exchanges = gate.exchanges();
        assert_eq!(exchanges[0].returned_ns, None);
        assert_eq!(exchanges[0].body, None);
        gate.begin_call();
        assert!(gate.exchanges().is_empty());
    }

    #[test]
    fn a_new_call_forgets_the_last_ones_calls_and_reason() {
        let gate = gate(true);
        let _ = gate.permit(&order());
        gate.begin_call();
        assert!(gate.calls().is_empty());
        assert_eq!(gate.take_held(), None);
        assert_eq!(gate.printed().len(), 1, "what was printed stays printed");
    }
}
