//! The one door into the tier from outside the crate.
//!
//! The tier is compiled inside this crate because the signed clients are `pub(crate)`. Another
//! repository (the one that owns the cases' bands, run directories and results) runs a case by
//! calling [`run`] with the case and the variables it reads, and reads what came back:
//!
//! ```ignore
//! let vars = Variables::process().with("VP_DRY_RUN", "1");
//! let summary = venue_ports::production::run(Case::Lv2aExchange, &vars).await?;
//! assert!(summary.is_clean());
//! ```
//!
//! Nothing here is new behaviour: [`run`] is what each case's `#[ignore]`d test does, in one
//! place, and those tests call it.

use crate::cex::CexTimings;
use crate::production::env::{Env, ProcessEnv};
use crate::production::gate::Echo;
use crate::production::guard::PRODUCTION_BINANCE_SPOT;
use crate::production::results::Tally;
use crate::production::{lv1_base, lv1_binance, lv2a_exchange, lv2b_chain, Run};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One case of the tier: a run against a venue, with its own settings and its own exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Case {
    /// LV1 on Binance's production spot host: reach, refusals and reads (B1 to B10).
    Lv1Binance,
    /// LV1 on Base mainnet nodes (C1 to C6).
    Lv1Base,
    /// LV2a: ten small market round trips on Binance, reconciled to the unit.
    Lv2aExchange,
    /// LV2b: small swaps through the router on Base, reconciled to the unit.
    Lv2bChain,
}

impl Case {
    /// The case's name as a run writes it and a command line spells it.
    pub fn key(self) -> &'static str {
        match self {
            Case::Lv1Binance => "lv1-binance",
            Case::Lv1Base => "lv1-base",
            Case::Lv2aExchange => "lv2a-exchange",
            Case::Lv2bChain => "lv2b-chain",
        }
    }
}

/// The variables a run reads: the process's environment, with whatever the caller sets over it. A
/// secret a run takes is gone afterwards, from the process's environment or from the overrides.
#[derive(Default)]
pub struct Variables {
    set: Mutex<BTreeMap<String, String>>,
}

impl Variables {
    /// The process's environment, unchanged.
    pub fn process() -> Self {
        Self::default()
    }

    /// `key` is `value` for this run, over the process's environment.
    pub fn with(self, key: &str, value: &str) -> Self {
        if let Ok(mut set) = self.set.lock() {
            set.insert(key.to_string(), value.to_string());
        }
        self
    }
}

impl Env for Variables {
    fn var(&self, key: &str) -> Option<String> {
        let overridden = self.set.lock().ok().and_then(|set| set.get(key).cloned());
        overridden
            .filter(|value| !value.is_empty())
            .or_else(|| ProcessEnv.var(key))
    }

    fn take(&self, key: &str) -> Option<String> {
        let overridden = self.set.lock().ok().and_then(|mut set| set.remove(key));
        match overridden.filter(|value| !value.is_empty()) {
            Some(value) => Some(value),
            None => ProcessEnv.take(key),
        }
    }
}

/// What a run came to: how many calls ended each way. The lines themselves are in `results.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The run's id, as every line of its results carries it.
    pub run_id: String,
    pub tally: Tally,
}

impl Summary {
    /// Whether the run's exit status is success: nothing failed, nothing was halted and nothing is
    /// unmapped.
    pub fn is_clean(&self) -> bool {
        self.tally.is_clean()
    }
}

/// Runs `case` with `vars`. An error is a run that could not start or could not go on (a setting
/// missing, a host the guard refuses, the spend cap): what it did before is in its results. A run
/// that went through and found a failure is `Ok`, and [`Summary::is_clean`] says so.
///
/// With `VP_DRY_RUN=1` every call is printed and none is sent.
pub async fn run(case: Case, vars: &Variables) -> Result<Summary> {
    match case {
        Case::Lv1Binance => {
            let run = Run::start(vars, "binance-spot", "api.binance.com", Echo::Stdout)?;
            let settings = lv1_binance::Settings::from_env(vars)?;
            let tally = lv1_binance::Lv1Binance::new(
                &run,
                vars,
                PRODUCTION_BINANCE_SPOT.to_string(),
                CexTimings::default(),
                settings,
            )
            .run_all()
            .await?;
            Ok(Summary {
                run_id: run.id.clone(),
                tally,
            })
        }
        Case::Lv1Base => {
            let run = Run::start(vars, "base", "base-mainnet", Echo::Stdout)?;
            let settings = lv1_base::Settings::from_env(vars)?;
            let tally = lv1_base::Lv1Base::new(&run, settings).run_all().await?;
            Ok(Summary {
                run_id: run.id.clone(),
                tally,
            })
        }
        Case::Lv2aExchange => {
            let run = Run::start(vars, "binance-spot", "api.binance.com", Echo::Stdout)?;
            let settings = lv2a_exchange::Settings::from_env(vars)?;
            let tally = lv2a_exchange::Lv2aExchange::new(
                &run,
                vars,
                PRODUCTION_BINANCE_SPOT.to_string(),
                CexTimings::default(),
                settings,
            )
            .run_all()
            .await?;
            Ok(Summary {
                run_id: run.id.clone(),
                tally,
            })
        }
        Case::Lv2bChain => {
            // The run reads the key's value first, so that no line can hold it.
            let run = Run::start(vars, "base", "base-mainnet", Echo::Stdout)?;
            let (settings, signer) = lv2b_chain::Settings::from_env(vars)?;
            let tally = lv2b_chain::Lv2bChain::new(&run, settings)
                .run_all(signer)
                .await?;
            Ok(Summary {
                run_id: run.id.clone(),
                tally,
            })
        }
    }
}
