//! `results.jsonl`: one object per call, the contract with whoever reads a
//! production run (`specs/V7-production-validation.md`, "Results").
//!
//! ```json
//! {"run_id":"…","case":"B7","venue":"binance-spot","host":"api.binance.com","started_ms":1791395227000,
//!  "request":{"method":"POST","path":"/api/v3/order","params":{"symbol":"AEROUSDT","side":"BUY"}},
//!  "outcome":{"kind":"refused","http_status":400,"code":-2010,"msg":"…"},
//!  "expected":{"kind":"refused","codes":[-2010]},
//!  "verdict":"pass","reason":"","body_file":"07-order-insufficient-balance.json","latency_ms":212,
//!  "references":null}
//! ```
//!
//! `references` is an object on a call that trades and `null` on every other.
//! A key, a secret, a signature or a `uid` is never written: the writer refuses
//! a line that holds one, and the run stops.

use crate::production::wire::Held;
use anyhow::{bail, Context, Result};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// What a call did, as the harness reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Ok,
    Refused,
    Lost,
    NotSent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The venue answered and the answer was read.
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The venue read the request and refused it.
    Refused {
        http_status: u16,
        code: Option<i64>,
        msg: String,
    },
    /// Sent, and no readable answer came back: the venue may have acted.
    Lost { msg: String },
    /// Nothing reached the venue.
    NotSent { msg: String },
}

impl Outcome {
    pub(crate) fn ok() -> Self {
        Outcome::Ok { note: None }
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Outcome::Ok { .. } => Kind::Ok,
            Outcome::Refused { .. } => Kind::Refused,
            Outcome::Lost { .. } => Kind::Lost,
            Outcome::NotSent { .. } => Kind::NotSent,
        }
    }

    /// The outcome of a call that ended in `err`, read from the error's chain
    /// by type and by the text every failed call has always begun with.
    pub(crate) fn of_error(err: &anyhow::Error) -> Self {
        if let Some(refusal) = crate::cex::refusal_of(err) {
            return Outcome::Refused {
                http_status: refusal.status,
                code: refusal.code,
                msg: refusal.msg.clone(),
            };
        }
        let said = format!("{err:#}");
        if err
            .downcast_ref::<crate::cex::OrderStateUnknown>()
            .is_some()
            || err
                .chain()
                .any(|layer| layer.to_string().starts_with("no readable answer: "))
        {
            return Outcome::Lost { msg: said };
        }
        Outcome::NotSent { msg: said }
    }
}

/// What a case expects: a kind, and for a refusal the codes the catalogue has
/// a row for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Expected {
    pub(crate) kind: Kind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) codes: Vec<i64>,
}

impl Expected {
    pub(crate) fn ok() -> Self {
        Self {
            kind: Kind::Ok,
            codes: Vec::new(),
        }
    }

    pub(crate) fn refused(codes: &[i64]) -> Self {
        Self {
            kind: Kind::Refused,
            codes: codes.to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Pass,
    Fail,
    /// The case could not run, and says why. Never a pass.
    Skipped,
    /// The run was stopped here: the halt file, or the ledger.
    Halted,
    /// A reply whose kind or code is in no row of the catalogue for the case:
    /// neither a pass nor a fail, and it fails the run until a row is added.
    Unmapped,
}

/// The verdict on `outcome` against `expected`, and why. `held` is what stopped
/// the call before it was sent, if anything did.
pub(crate) fn judge(
    expected: &Expected,
    outcome: &Outcome,
    held: Option<&Held>,
) -> (Verdict, String) {
    match held {
        Some(Held::DryRun) => return (Verdict::Skipped, "dry run: nothing was sent".to_string()),
        Some(Held::Halted(file)) => {
            return (Verdict::Halted, format!("the halt file {file} is present"))
        }
        Some(Held::Unauthorised(why)) => return (Verdict::Halted, format!("the ledger: {why}")),
        Some(Held::Failed(why)) => return (Verdict::Fail, format!("the harness failed: {why}")),
        None => {}
    }
    match (expected.kind, outcome) {
        (Kind::Ok, Outcome::Ok { .. }) => (Verdict::Pass, String::new()),
        (Kind::Refused, Outcome::Ok { .. }) => (
            Verdict::Fail,
            "the venue accepted a request it must refuse".to_string(),
        ),
        (
            Kind::Refused,
            Outcome::Refused {
                code, http_status, ..
            },
        ) => {
            if expected.codes.is_empty() {
                (
                    Verdict::Pass,
                    format!("refused (HTTP {http_status}, code {code:?}); the code is recorded"),
                )
            } else if code.is_some_and(|code| expected.codes.contains(&code)) {
                (Verdict::Pass, String::new())
            } else {
                (
                    Verdict::Unmapped,
                    format!(
                        "refused with code {code:?} (HTTP {http_status}), which is in no row for \
                         this case (rows: {:?})",
                        expected.codes
                    ),
                )
            }
        }
        (Kind::Refused, other) => (
            Verdict::Unmapped,
            format!(
                "expected a refusal, got a call that was {:?}, which is in no row for this case",
                other.kind()
            ),
        ),
        (
            Kind::Ok,
            Outcome::Refused {
                code,
                http_status,
                msg,
            },
        ) => (
            Verdict::Fail,
            format!("expected an answer, got a refusal (HTTP {http_status}, code {code:?}): {msg}"),
        ),
        (Kind::Ok, other) => (
            Verdict::Fail,
            format!("expected an answer, got a call that was {:?}", other.kind()),
        ),
        (expected_kind, other) if expected_kind == other.kind() => (Verdict::Pass, String::new()),
        (expected_kind, other) => (
            Verdict::Unmapped,
            format!(
                "expected {expected_kind:?}, got {:?}, which is in no row for this case",
                other.kind()
            ),
        ),
    }
}

/// The parameters of a request in the order the caller gave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Params(pub(crate) Vec<(String, String)>);

impl Serialize for Params {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RequestRecord {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) params: Params,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ResultLine {
    pub(crate) run_id: String,
    pub(crate) case: String,
    pub(crate) venue: String,
    pub(crate) host: String,
    pub(crate) started_ms: u64,
    pub(crate) request: RequestRecord,
    pub(crate) outcome: Outcome,
    pub(crate) expected: Expected,
    pub(crate) verdict: Verdict,
    pub(crate) reason: String,
    pub(crate) body_file: Option<String>,
    pub(crate) latency_ms: u64,
    /// An object on a call that trades; `null` on every other.
    pub(crate) references: Option<serde_json::Value>,
}

/// How many calls ended each way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) pass: u32,
    pub(crate) fail: u32,
    pub(crate) skipped: u32,
    pub(crate) halted: u32,
    pub(crate) unmapped: u32,
}

impl Tally {
    /// Whether the run's exit status is success: nothing failed, nothing was
    /// halted and nothing is unmapped.
    pub(crate) fn is_clean(&self) -> bool {
        self.fail == 0 && self.halted == 0 && self.unmapped == 0
    }
}

pub(crate) struct Results {
    inner: Mutex<Inner>,
    /// Text that must never appear in a line: keys and secrets.
    forbidden: Vec<String>,
}

struct Inner {
    file: File,
    tally: Tally,
}

impl Results {
    /// Makes `results.jsonl` in `dir` (made if it is missing). A file from an
    /// earlier run is evidence and is not overwritten.
    pub(crate) fn create(dir: &Path, forbidden: Vec<String>) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        let path = dir.join("results.jsonl");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "making {}: a results file from an earlier run is not overwritten, so name \
                     a new VP_OUT",
                    path.display()
                )
            })?;
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                tally: Tally::default(),
            }),
            forbidden: forbidden
                .into_iter()
                .map(|secret| secret.trim().to_string())
                .filter(|secret| !secret.is_empty())
                .collect(),
        })
    }

    /// Appends `line`, and counts its verdict. A line that holds a key, a
    /// secret, a signature or a `uid` is not written.
    pub(crate) fn write(&self, line: &ResultLine) -> Result<()> {
        let text = serde_json::to_string(line).context("serialising a result")?;
        if let Some(why) = self.leak_in(&text) {
            bail!(
                "refusing to write a result for {}: it holds {why}",
                line.case
            );
        }
        let mut inner = self.inner.lock().unwrap();
        writeln!(inner.file, "{text}").context("writing a result")?;
        inner.file.flush().context("writing a result")?;
        match line.verdict {
            Verdict::Pass => inner.tally.pass += 1,
            Verdict::Fail => inner.tally.fail += 1,
            Verdict::Skipped => inner.tally.skipped += 1,
            Verdict::Halted => inner.tally.halted += 1,
            Verdict::Unmapped => inner.tally.unmapped += 1,
        }
        Ok(())
    }

    pub(crate) fn tally(&self) -> Tally {
        self.inner.lock().unwrap().tally
    }

    fn leak_in(&self, text: &str) -> Option<&'static str> {
        if self.forbidden.iter().any(|secret| text.contains(secret)) {
            return Some("a key or a secret");
        }
        if text.contains("signature=") {
            return Some("a signature");
        }
        if text.contains("\"uid\"") {
            return Some("a uid");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(case: &str) -> ResultLine {
        ResultLine {
            run_id: "run-1".to_string(),
            case: case.to_string(),
            venue: "binance-spot".to_string(),
            host: "api.binance.com".to_string(),
            started_ms: 1_791_395_227_000,
            request: RequestRecord {
                method: "POST".to_string(),
                path: "/api/v3/order".to_string(),
                params: Params(vec![
                    ("symbol".to_string(), "AEROUSDT".to_string()),
                    ("side".to_string(), "BUY".to_string()),
                    ("type".to_string(), "MARKET".to_string()),
                    ("quantity".to_string(), "10".to_string()),
                ]),
            },
            outcome: Outcome::Refused {
                http_status: 400,
                code: Some(-2010),
                msg: "Account has insufficient balance for requested action.".to_string(),
            },
            expected: Expected::refused(&[-2010]),
            verdict: Verdict::Pass,
            reason: String::new(),
            body_file: Some("07-order-insufficient-balance.json".to_string()),
            latency_ms: 212,
            references: None,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "venue-ports-results-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// The spec's example line, key for key.
    #[test]
    fn a_line_has_the_keys_of_the_contract_in_the_order_the_caller_gave_the_params() {
        let text = serde_json::to_string(&line("B7")).unwrap();
        assert_eq!(
            text,
            r#"{"run_id":"run-1","case":"B7","venue":"binance-spot","host":"api.binance.com","started_ms":1791395227000,"request":{"method":"POST","path":"/api/v3/order","params":{"symbol":"AEROUSDT","side":"BUY","type":"MARKET","quantity":"10"}},"outcome":{"kind":"refused","http_status":400,"code":-2010,"msg":"Account has insufficient balance for requested action."},"expected":{"kind":"refused","codes":[-2010]},"verdict":"pass","reason":"","body_file":"07-order-insufficient-balance.json","latency_ms":212,"references":null}"#
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value["references"].is_null(), "null, and present");
    }

    #[test]
    fn the_four_outcomes_and_five_verdicts_are_spelt_as_the_contract_spells_them() {
        assert_eq!(
            serde_json::to_value(Outcome::ok()).unwrap(),
            json!({"kind": "ok"})
        );
        assert_eq!(
            serde_json::to_value(Outcome::Lost { msg: "x".into() }).unwrap(),
            json!({"kind": "lost", "msg": "x"})
        );
        assert_eq!(
            serde_json::to_value(Outcome::NotSent { msg: "x".into() }).unwrap(),
            json!({"kind": "not_sent", "msg": "x"})
        );
        for (verdict, text) in [
            (Verdict::Pass, "pass"),
            (Verdict::Fail, "fail"),
            (Verdict::Skipped, "skipped"),
            (Verdict::Halted, "halted"),
            (Verdict::Unmapped, "unmapped"),
        ] {
            assert_eq!(serde_json::to_value(verdict).unwrap(), json!(text));
        }
    }

    #[test]
    fn an_error_is_read_as_a_refusal_a_lost_answer_or_a_request_never_sent() {
        let refused: anyhow::Error = crate::cex::VenueRefusal {
            status: 400,
            code: Some(-1013),
            msg: "Filter failure: NOTIONAL".to_string(),
        }
        .into();
        assert_eq!(
            Outcome::of_error(&refused.context("placing it")),
            Outcome::Refused {
                http_status: 400,
                code: Some(-1013),
                msg: "Filter failure: NOTIONAL".to_string()
            }
        );
        assert_eq!(
            Outcome::of_error(&anyhow::anyhow!("no readable answer: timed out")).kind(),
            Kind::Lost
        );
        assert_eq!(
            Outcome::of_error(&anyhow::anyhow!("timed out").context("no readable answer: GET /x"))
                .kind(),
            Kind::Lost
        );
        let unknown: anyhow::Error = crate::cex::OrderStateUnknown {
            symbol: "AEROUSDT".to_string(),
            client_order_id: "vp-1".to_string(),
            order_ref: None,
        }
        .into();
        assert_eq!(Outcome::of_error(&unknown).kind(), Kind::Lost);
        assert_eq!(
            Outcome::of_error(&anyhow::anyhow!("not sent: dry run")).kind(),
            Kind::NotSent
        );
    }

    fn refused(code: Option<i64>) -> Outcome {
        Outcome::Refused {
            http_status: 400,
            code,
            msg: "no".to_string(),
        }
    }

    #[test]
    fn the_expected_refusal_passes_and_an_accepted_one_fails() {
        let expected = Expected::refused(&[-1013]);
        assert_eq!(
            judge(&expected, &refused(Some(-1013)), None),
            (Verdict::Pass, String::new())
        );
        let (verdict, why) = judge(&expected, &Outcome::ok(), None);
        assert_eq!(verdict, Verdict::Fail);
        assert!(why.contains("accepted"), "{why}");
    }

    /// A refusal the catalogue has no row for is neither a pass nor a fail.
    #[test]
    fn a_reply_with_no_row_is_unmapped_and_says_which_code() {
        let expected = Expected::refused(&[-1013]);
        let (verdict, why) = judge(&expected, &refused(Some(-1111)), None);
        assert_eq!(verdict, Verdict::Unmapped);
        assert!(why.contains("-1111") && why.contains("-1013"), "{why}");
        let (verdict, _) = judge(&expected, &refused(None), None);
        assert_eq!(
            verdict,
            Verdict::Unmapped,
            "a refusal with no code has no row"
        );
        let (verdict, why) = judge(
            &expected,
            &Outcome::Lost {
                msg: "timed out".to_string(),
            },
            None,
        );
        assert_eq!(verdict, Verdict::Unmapped);
        assert!(why.contains("Lost"), "{why}");
    }

    #[test]
    fn a_refusal_expected_with_no_codes_records_whichever_it_gets() {
        let (verdict, why) = judge(&Expected::refused(&[]), &refused(Some(-1102)), None);
        assert_eq!(verdict, Verdict::Pass);
        assert!(why.contains("-1102") && why.contains("recorded"), "{why}");
    }

    #[test]
    fn a_read_that_must_answer_fails_when_it_does_not() {
        let expected = Expected::ok();
        assert_eq!(judge(&expected, &Outcome::ok(), None).0, Verdict::Pass);
        let (verdict, why) = judge(&expected, &refused(Some(-2015)), None);
        assert_eq!(verdict, Verdict::Fail);
        assert!(why.contains("-2015"), "{why}");
        assert_eq!(
            judge(&expected, &Outcome::NotSent { msg: "x".into() }, None).0,
            Verdict::Fail
        );
    }

    #[test]
    fn a_call_that_was_held_back_is_skipped_halted_or_failed_never_passed() {
        let expected = Expected::refused(&[-2010]);
        let not_sent = Outcome::NotSent {
            msg: "not sent: dry run".to_string(),
        };
        assert_eq!(
            judge(&expected, &not_sent, Some(&Held::DryRun)).0,
            Verdict::Skipped
        );
        assert_eq!(
            judge(&expected, &not_sent, Some(&Held::Halted("STOP".into()))).0,
            Verdict::Halted
        );
        assert_eq!(
            judge(
                &expected,
                &not_sent,
                Some(&Held::Unauthorised("cap".into()))
            )
            .0,
            Verdict::Halted
        );
        assert_eq!(
            judge(&expected, &not_sent, Some(&Held::Failed("disk".into()))).0,
            Verdict::Fail
        );
    }

    #[test]
    fn lines_are_appended_and_counted() {
        let dir = scratch("append");
        let results = Results::create(&dir, vec![]).unwrap();
        results.write(&line("B1")).unwrap();
        let mut unmapped = line("B2");
        unmapped.verdict = Verdict::Unmapped;
        results.write(&unmapped).unwrap();

        let text = std::fs::read_to_string(dir.join("results.jsonl")).unwrap();
        let cases: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["case"].to_string())
            .collect();
        assert_eq!(cases, ["\"B1\"", "\"B2\""]);
        let tally = results.tally();
        assert_eq!((tally.pass, tally.unmapped), (1, 1));
        assert!(!tally.is_clean(), "an unmapped reply fails the run");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_with_only_passes_and_skips_is_clean() {
        let tally = Tally {
            pass: 9,
            skipped: 3,
            ..Tally::default()
        };
        assert!(tally.is_clean());
        for dirty in [
            Tally { fail: 1, ..tally },
            Tally { halted: 1, ..tally },
            Tally {
                unmapped: 1,
                ..tally
            },
        ] {
            assert!(!dirty.is_clean());
        }
    }

    #[test]
    fn a_results_file_from_an_earlier_run_is_not_overwritten() {
        let dir = scratch("keep");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("results.jsonl"), "evidence").unwrap();

        let err = Results::create(&dir, vec![]).err().expect("refused");

        assert!(format!("{err:#}").contains("not overwritten"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(dir.join("results.jsonl")).unwrap(),
            "evidence"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A key, a secret, a signature or a `uid` is never written to it.
    #[test]
    fn a_line_with_a_key_a_secret_a_signature_or_a_uid_is_refused() {
        let dir = scratch("leak");
        let results = Results::create(
            &dir,
            vec![
                "AKEY1234".to_string(),
                "  ".to_string(),
                "S3CRET99".to_string(),
            ],
        )
        .unwrap();

        let mut key = line("B1");
        key.reason = "the key AKEY1234 was refused".to_string();
        let mut secret = line("B2");
        secret.outcome = Outcome::NotSent {
            msg: "S3CRET99".to_string(),
        };
        let mut signature = line("B3");
        signature.reason = "GET /x?a=1&signature=abcd".to_string();
        let mut uid = line("B4");
        uid.references = Some(json!({"uid": 354937868}));

        for (bad, what) in [
            (key, "a key or a secret"),
            (secret, "a key or a secret"),
            (signature, "a signature"),
            (uid, "a uid"),
        ] {
            let err = results.write(&bad).unwrap_err().to_string();
            assert!(err.contains(what) && err.contains(&bad.case), "{err}");
        }
        results.write(&line("B5")).unwrap();
        let text = std::fs::read_to_string(dir.join("results.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1, "only the clean line was written");
        assert!(!text.contains("AKEY1234") && !text.contains("S3CRET99"));
        assert_eq!(results.tally().pass, 1, "a refused line is not counted");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
