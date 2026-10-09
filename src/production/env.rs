//! Where a production run reads its settings from.
//!
//! The tests of the harness never touch the process environment (the test
//! threads share it): they hand the harness a [`MapEnv`]. An ignored test
//! hands it [`ProcessEnv`]. A variable that is set to nothing is not set.

use anyhow::{anyhow, bail, Context, Result};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Mutex;

pub(crate) trait Env {
    /// The variable's value, or `None` when it is unset or empty.
    fn var(&self, key: &str) -> Option<String>;

    /// As [`Env::var`], and the variable is gone afterwards: for a secret read
    /// once, which must not stay in the process's environment.
    fn take(&self, key: &str) -> Option<String>;
}

/// The environment of the process.
pub(crate) struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }

    fn take(&self, key: &str) -> Option<String> {
        let value = self.var(key);
        std::env::remove_var(key);
        value
    }
}

/// A fixed set of variables, for a test.
#[derive(Default)]
pub(crate) struct MapEnv(Mutex<BTreeMap<String, String>>);

impl MapEnv {
    pub(crate) fn new(vars: &[(&str, &str)]) -> Self {
        Self(Mutex::new(
            vars.iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        ))
    }
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .filter(|value| !value.is_empty())
    }

    fn take(&self, key: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .remove(key)
            .filter(|value| !value.is_empty())
    }
}

/// The variable, or an error naming it. There is no default for a credential,
/// a cap, a file or a host.
pub(crate) fn required(env: &dyn Env, key: &str) -> Result<String> {
    env.var(key)
        .ok_or_else(|| anyhow!("{key} is not set and has no default"))
}

/// A decimal from the variable, or `default` when it is unset (`None` makes it
/// required). A value that is not a positive decimal is an error naming the
/// variable: a cap of zero or less is not a cap.
pub(crate) fn positive_decimal(
    env: &dyn Env,
    key: &str,
    default: Option<Decimal>,
) -> Result<Decimal> {
    let value = match env.var(key) {
        Some(text) => Decimal::from_str(text.trim())
            .with_context(|| format!("{key} is {text:?}, which is not a decimal"))?,
        None => default.ok_or_else(|| anyhow!("{key} is not set and has no default"))?,
    };
    if value <= Decimal::ZERO {
        bail!("{key} is {value}: it must be greater than zero");
    }
    Ok(value)
}

/// Whether the variable is exactly `1`.
pub(crate) fn flag(env: &dyn Env, key: &str) -> bool {
    env.var(key).as_deref() == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_variable_is_an_unset_one_and_a_taken_one_is_gone() {
        let env = MapEnv::new(&[("A", ""), ("B", "x")]);
        assert_eq!(env.var("A"), None);
        assert_eq!(env.var("B").as_deref(), Some("x"));
        assert_eq!(env.take("B").as_deref(), Some("x"));
        assert_eq!(env.var("B"), None, "taken once");
        assert!(required(&env, "A")
            .unwrap_err()
            .to_string()
            .contains("A is not set"));
    }

    /// The one test that sets a variable of the process, under a name nothing
    /// else reads.
    #[test]
    fn the_process_environment_is_read_and_a_secret_taken_from_it_is_gone() {
        let key = format!("VENUE_PORTS_TEST_TAKEN_ONCE_{}", std::process::id());
        std::env::set_var(&key, "s3cret");
        assert_eq!(ProcessEnv.var(&key).as_deref(), Some("s3cret"));
        assert_eq!(ProcessEnv.take(&key).as_deref(), Some("s3cret"));
        assert_eq!(ProcessEnv.var(&key), None, "removed from the process");
        assert_eq!(std::env::var(&key).ok(), None);
        assert_eq!(ProcessEnv.take(&key), None);
    }

    #[test]
    fn a_cap_is_a_positive_decimal_with_a_named_failure() {
        let env = MapEnv::new(&[("CAP", "12.5"), ("ZERO", "0"), ("WORDS", "twelve")]);
        assert_eq!(
            positive_decimal(&env, "CAP", None).unwrap(),
            Decimal::from_str("12.5").unwrap()
        );
        assert_eq!(
            positive_decimal(&env, "UNSET", Some(Decimal::from(12))).unwrap(),
            Decimal::from(12)
        );
        assert!(positive_decimal(&env, "UNSET", None).is_err());
        let zero = positive_decimal(&env, "ZERO", None)
            .unwrap_err()
            .to_string();
        assert!(
            zero.contains("ZERO") && zero.contains("greater than zero"),
            "{zero}"
        );
        let words = format!("{:#}", positive_decimal(&env, "WORDS", None).unwrap_err());
        assert!(
            words.contains("WORDS") && words.contains("not a decimal"),
            "{words}"
        );
    }

    #[test]
    fn a_flag_is_one_and_nothing_else() {
        let env = MapEnv::new(&[("ON", "1"), ("TRUE", "true"), ("ZERO", "0")]);
        assert!(flag(&env, "ON"));
        assert!(!flag(&env, "TRUE"));
        assert!(!flag(&env, "ZERO"));
        assert!(!flag(&env, "UNSET"));
    }
}
