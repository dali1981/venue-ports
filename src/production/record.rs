//! Record mode (`VP_RECORD_DIR`): each reply a run gets from the venue is
//! saved as `<step>-<name>.json` with `<step>-<name>.status` beside it, the
//! body exactly as the venue sent it, and the HTTP status.
//!
//! **The only edit is the account's `uid`**, whose value becomes
//! `"REDACTED-UID"` ([`redact_uid`]); every other byte is unchanged. The
//! directory is one recording: the run refuses to start if it exists.
//!
//! A call is named by the case before it is made ([`Recorder::name_next`]).
//! Its own reply is `<name>.json`; a call that makes more than one request
//! (an order, then the order's status, then its trades) saves the later ones
//! as `<name>-2.json`, `<name>-3.json`. The venue's clock replies are not
//! saved: they would take the name of the call they come before. A reply no
//! case named is saved as `unnamed-<n>-<path>.json`.
//!
//! A transaction a call sends is kept beside those, since the node's replies
//! never hold it ([`Recorder::keep`]): the signed bytes as
//! `<step>-<name>-signed.hex` (written before the send, so a swap that never
//! lands has them) and the node's whole reply to the send as
//! `<step>-<name>-send-reply.json`. They are not replies of a venue: no
//! `.status`, and they never become a line's `body_file`.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What replaces the value of a `uid`.
pub(crate) const REDACTED_UID: &str = "REDACTED-UID";

/// `body` with the value of every `"uid"` key replaced by `"REDACTED-UID"`
/// (a JSON string, whatever the value was: Binance sends a number). Nothing
/// else is touched, whitespace included.
pub(crate) fn redact_uid(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let (mut search_from, mut copied) = (0, 0);
    while let Some(found) = body[search_from..].find("\"uid\"") {
        let key = search_from + found;
        search_from = key + "\"uid\"".len();
        // A quote that closes an escaped one is part of a string, not a key.
        if key > 0 && bytes[key - 1] == b'\\' {
            continue;
        }
        let mut at = search_from;
        let skip_space = |mut at: usize| {
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            at
        };
        at = skip_space(at);
        if bytes.get(at) != Some(&b':') {
            continue;
        }
        let value_start = skip_space(at + 1);
        let value_end = match bytes.get(value_start) {
            None => continue,
            Some(b'"') => {
                let mut end = value_start + 1;
                while let Some(&byte) = bytes.get(end) {
                    match byte {
                        b'\\' => end += 2,
                        b'"' => break,
                        _ => end += 1,
                    }
                }
                (end + 1).min(bytes.len())
            }
            Some(_) => {
                let mut end = value_start;
                while bytes
                    .get(end)
                    .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b',' | b'}' | b']'))
                {
                    end += 1;
                }
                end
            }
        };
        if value_end == value_start {
            continue;
        }
        out.push_str(&body[copied..value_start]);
        out.push('"');
        out.push_str(REDACTED_UID);
        out.push('"');
        copied = value_end;
        search_from = value_end;
    }
    out.push_str(&body[copied..]);
    out
}

#[derive(Default)]
struct State {
    /// The name the next reply is saved under, and how many it has had.
    current: Option<(String, u32)>,
    unnamed: u32,
    written: BTreeSet<String>,
    /// The first file the current call wrote.
    first_file: Option<String>,
    /// How many of each kind of thing the current call sent has been kept.
    kept: BTreeMap<&'static str, u32>,
    /// The first thing that went wrong. A run that could not record what it was
    /// asked to record stops.
    failure: Option<String>,
}

/// What a call sends, or what the node answers to the send, kept beside the
/// call's replies: a transaction is ours before it is anyone's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The signed transaction, as the hex `eth_sendRawTransaction` was given.
    Signed,
    /// The node's whole JSON-RPC reply to that send.
    SendReply,
}

pub(crate) struct Recorder {
    dir: PathBuf,
    state: Mutex<State>,
}

impl Recorder {
    /// Makes `dir`, which must not exist: a directory is one recording.
    pub(crate) fn create(dir: &Path) -> Result<Self> {
        if dir.exists() {
            bail!(
                "VP_RECORD_DIR {} exists: a recording is one directory written once, so name a \
                 new one",
                dir.display()
            );
        }
        if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("making {}", parent.display()))?;
        }
        std::fs::create_dir(dir).with_context(|| format!("making {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(State::default()),
        })
    }

    /// The next call's replies are saved as `stem`, `stem-2`, … `stem` is
    /// `<step>-<name>`: letters, digits, `-`, `_` and `.`.
    pub(crate) fn name_next(&self, stem: &str) {
        let mut state = self.state.lock().unwrap();
        let valid = !stem.is_empty()
            && stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !valid {
            state
                .failure
                .get_or_insert(format!("{stem:?} is not a name a recording can use"));
            return;
        }
        state.current = Some((stem.to_string(), 0));
        state.first_file = None;
        state.kept.clear();
    }

    /// The current call is over: later replies are no one's.
    pub(crate) fn end_call(&self) {
        let mut state = self.state.lock().unwrap();
        state.current = None;
        state.kept.clear();
    }

    /// The file the current (or last) named call's first reply went to.
    pub(crate) fn first_file(&self) -> Option<String> {
        self.state.lock().unwrap().first_file.clone()
    }

    /// What went wrong, if anything did.
    pub(crate) fn failure(&self) -> Option<String> {
        self.state.lock().unwrap().failure.clone()
    }

    /// Keeps what the current call sent, or the node's reply to the send, as
    /// `<stem>-signed.hex` and `<stem>-send-reply.json` (`-2`, `-3` for a call that
    /// sends more than once). They are not replies of the venue: no `.status`, and
    /// not the call's `first_file`. A send no case named is
    /// `unnamed-<n>-signed.hex`, numbered with the replies nobody named.
    pub(crate) fn keep(&self, what: Kept, contents: &str) {
        let (kind, extension) = match what {
            Kept::Signed => ("signed", "hex"),
            Kept::SendReply => ("send-reply", "json"),
        };
        let mut state = self.state.lock().unwrap();
        let stem = match state.current.as_ref() {
            Some((stem, _)) => stem.clone(),
            None => {
                state.unnamed += 1;
                format!("unnamed-{}", state.unnamed)
            }
        };
        let count = state.kept.entry(kind).or_insert(0);
        *count += 1;
        let file = if *count == 1 {
            format!("{stem}-{kind}.{extension}")
        } else {
            format!("{stem}-{kind}-{count}.{extension}")
        };
        if !state.written.insert(file.clone()) {
            state
                .failure
                .get_or_insert(format!("{file} was recorded twice"));
            return;
        }
        if let Err(err) = std::fs::write(self.dir.join(&file), contents) {
            state
                .failure
                .get_or_insert(format!("writing {file}: {err}"));
        }
    }

    /// Saves a reply. The venue's clock replies are skipped.
    pub(crate) fn observe(&self, path: &str, status: u16, body: &str) {
        if path.ends_with("/time") {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let stem = match state.current.as_mut() {
            Some((stem, count)) => {
                *count += 1;
                if *count == 1 {
                    stem.clone()
                } else {
                    format!("{stem}-{count}")
                }
            }
            None => {
                state.unnamed += 1;
                let slug: String = path
                    .trim_matches('/')
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                format!("unnamed-{}-{slug}", state.unnamed)
            }
        };
        if !state.written.insert(stem.clone()) {
            state
                .failure
                .get_or_insert(format!("{stem} was recorded twice"));
            return;
        }
        let json = self.dir.join(format!("{stem}.json"));
        let status_file = self.dir.join(format!("{stem}.status"));
        let written = std::fs::write(&json, redact_uid(body))
            .and_then(|()| std::fs::write(&status_file, format!("{status}\n")));
        match written {
            Ok(()) => {
                state.first_file.get_or_insert(format!("{stem}.json"));
            }
            Err(err) => {
                state
                    .failure
                    .get_or_insert(format!("writing {}: {err}", json.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of `GET /api/v3/account`, with its `uid` as Binance sends it:
    /// a number, with the whitespace and key order the venue used.
    const ACCOUNT: &str = "{\n    \"makerCommission\": 15,\n    \"canTrade\": true,\n    \
        \"updateTime\": 123456789,\n    \"accountType\": \"SPOT\",\n    \"balances\": [\n        \
        {\"asset\": \"BTC\", \"free\": \"4723846.89208129\", \"locked\": \"0.00000000\"}\n    ],\n    \
        \"permissions\": [\"SPOT\"],\n    \"uid\": 354937868\n}";

    #[test]
    fn the_uid_is_replaced_and_every_other_byte_is_unchanged() {
        let redacted = redact_uid(ACCOUNT);
        assert!(redacted.contains("\"uid\": \"REDACTED-UID\""), "{redacted}");
        assert!(!redacted.contains("354937868"));
        // Putting the original value back gives the original body, byte for byte.
        assert_eq!(redacted.replace("\"REDACTED-UID\"", "354937868"), ACCOUNT);
        // And it is still JSON.
        serde_json::from_str::<serde_json::Value>(&redacted).unwrap();
    }

    #[test]
    fn a_uid_in_any_position_or_form_is_redacted_and_only_a_uid() {
        for (body, expected) in [
            (r#"{"uid":1}"#, r#"{"uid":"REDACTED-UID"}"#),
            (r#"{"uid" : 1 }"#, r#"{"uid" : "REDACTED-UID" }"#),
            (
                r#"{"uid":"354937868","x":1}"#,
                r#"{"uid":"REDACTED-UID","x":1}"#,
            ),
            (
                r#"{"a":{"uid":7},"uid":8}"#,
                r#"{"a":{"uid":"REDACTED-UID"},"uid":"REDACTED-UID"}"#,
            ),
            (r#"[{"uid":7,"b":1}]"#, r#"[{"uid":"REDACTED-UID","b":1}]"#),
            // Not a uid: another key, a string that says so, an escaped quote.
            (r#"{"subUid":7,"puid":8}"#, r#"{"subUid":7,"puid":8}"#),
            (r#"{"asset":"uid","x":1}"#, r#"{"asset":"uid","x":1}"#),
            (r#"{"msg":"bad \"uid\": 5"}"#, r#"{"msg":"bad \"uid\": 5"}"#),
            // Nothing to do.
            ("", ""),
            ("Forbidden", "Forbidden"),
            (r#"{"uid":"#, r#"{"uid":"#),
        ] {
            assert_eq!(redact_uid(body), expected, "{body}");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact_uid(ACCOUNT);
        assert_eq!(redact_uid(&once), once);
    }

    #[test]
    fn a_body_that_is_not_json_is_kept_to_the_byte() {
        let html = "<html>\r\n<head><title>403 Forbidden</title></head>\u{00e9}\u{2014}\r\n</html>";
        assert_eq!(redact_uid(html), html);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "venue-ports-record-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_reply_is_saved_by_name_with_its_status_and_the_clock_is_not() {
        let dir = scratch("named");
        let recorder = Recorder::create(&dir).unwrap();

        recorder.name_next("01-account");
        recorder.observe("/api/v3/time", 200, r#"{"serverTime":1}"#);
        recorder.observe("/api/v3/account", 200, ACCOUNT);
        recorder.end_call();

        assert_eq!(recorder.first_file().as_deref(), Some("01-account.json"));
        assert_eq!(
            std::fs::read_to_string(dir.join("01-account.json")).unwrap(),
            redact_uid(ACCOUNT)
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("01-account.status")).unwrap(),
            "200\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no clock file");
        assert_eq!(recorder.failure(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_call_that_makes_several_requests_saves_each_and_a_reply_nobody_named_is_kept() {
        let dir = scratch("several");
        let recorder = Recorder::create(&dir).unwrap();

        recorder.name_next("05-order");
        recorder.observe("/api/v3/order", 200, "{}");
        recorder.observe("/api/v3/order", 200, "{\"status\":\"FILLED\"}");
        recorder.observe("/api/v3/myTrades", 200, "[]");
        recorder.end_call();
        recorder.observe("/api/v3/account", 200, "{}");

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "05-order-2.json",
                "05-order-2.status",
                "05-order-3.json",
                "05-order-3.status",
                "05-order.json",
                "05-order.status",
                "unnamed-1-api-v3-account.json",
                "unnamed-1-api-v3-account.status",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_a_call_sent_is_kept_beside_its_replies_numbered_like_them() {
        let dir = scratch("kept");
        let recorder = Recorder::create(&dir).unwrap();

        recorder.name_next("001-swap");
        recorder.keep(Kept::Signed, "0xaa");
        recorder.keep(
            Kept::SendReply,
            r#"{"id":1,"jsonrpc":"2.0","result":"0x11"}"#,
        );
        // A second send in the call is a second pair.
        recorder.keep(Kept::Signed, "0xbb");
        recorder.keep(Kept::SendReply, r#"{"error":{"code":-32000}}"#);
        recorder.end_call();
        // A send no case named is kept too, under the same counter as a reply.
        recorder.keep(Kept::Signed, "0xcc");

        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("001-swap-signed.hex"), "0xaa");
        assert_eq!(read("001-swap-signed-2.hex"), "0xbb");
        assert_eq!(
            read("001-swap-send-reply.json"),
            r#"{"id":1,"jsonrpc":"2.0","result":"0x11"}"#
        );
        assert_eq!(
            read("001-swap-send-reply-2.json"),
            r#"{"error":{"code":-32000}}"#
        );
        assert_eq!(read("unnamed-1-signed.hex"), "0xcc");
        // They are not replies: no status file, and none of them is `first_file`.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 5);
        assert_eq!(recorder.first_file(), None);
        assert_eq!(recorder.failure(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_kept_file_is_never_overwritten() {
        let dir = scratch("kept-twice");
        let recorder = Recorder::create(&dir).unwrap();
        recorder.name_next("001-swap");
        recorder.keep(Kept::Signed, "0xaa");
        recorder.end_call();
        recorder.name_next("001-swap");
        recorder.keep(Kept::Signed, "0xbb");
        assert!(recorder.failure().unwrap().contains("recorded twice"));
        assert_eq!(
            std::fs::read_to_string(dir.join("001-swap-signed.hex")).unwrap(),
            "0xaa"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_recording_directory_that_exists_is_refused_and_left_alone() {
        let dir = scratch("exists");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.json"), "kept").unwrap();

        let err = Recorder::create(&dir).err().expect("refused").to_string();

        assert!(
            err.contains("a recording is one directory written once"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.json")).unwrap(),
            "kept"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_a_file_cannot_have_and_a_name_used_twice_are_failures_not_overwrites() {
        let dir = scratch("failures");
        let recorder = Recorder::create(&dir).unwrap();
        recorder.name_next("../escape");
        assert!(recorder.failure().unwrap().contains("../escape"));

        let recorder = Recorder::create(&scratch("twice")).unwrap();
        recorder.name_next("01-a");
        recorder.observe("/x", 200, "first");
        recorder.name_next("01-a");
        recorder.observe("/x", 200, "second");
        assert!(recorder.failure().unwrap().contains("recorded twice"));
        let first = std::fs::read_to_string(recorder.dir.join("01-a.json")).unwrap();
        assert_eq!(first, "first", "never overwritten");
        std::fs::remove_dir_all(&recorder.dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
