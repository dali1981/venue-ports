//! The seam between the signed Binance client and the production harness.
//!
//! The client (`cex::binance::client`, test builds only) asks its [`Wire`] for
//! permission before it makes a call, and tells it every reply as the bytes
//! the venue sent. Nothing here is compiled into the library: the production
//! tier is a `#[cfg(test)]` module, because the signed client is `pub(crate)`.

use reqwest::Method;
use std::fmt;

/// One call, as a caller asked for it: the method, the path and the
/// parameters in the order given. The API key is a header and is never part
/// of a call; `timestamp`, `recvWindow` and `signature` are added when the
/// request is signed, after the harness has seen the call, so they are not
/// either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) params: Vec<(String, String)>,
}

impl Call {
    pub(crate) fn new(method: &Method, path: &str, params: &[(&str, String)]) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            params: params
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        }
    }

    /// Whether this call places an order on the matching engine: spot's or
    /// futures' `POST …/order`, and not `…/order/test`, which validates only.
    pub(crate) fn places_an_order(&self) -> bool {
        self.method == "POST" && matches!(self.path.as_str(), "/api/v3/order" | "/fapi/v1/order")
    }
}

/// `POST /api/v3/order symbol=AEROUSDT side=BUY`: what a dry run prints.
impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.path)?;
        for (key, value) in &self.params {
            write!(f, " {key}={value}")?;
        }
        Ok(())
    }
}

/// Why a call was not sent. The client turns every one into
/// `ApiError::NotSent`, so a caller reads it as it reads a connection that
/// could not be opened: nothing reached the venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Held {
    /// `VP_DRY_RUN=1`: the call was printed and not sent.
    DryRun,
    /// The halt file is present; the string names it.
    Halted(String),
    /// An order that no ledger check came before, or that the ledger refused.
    Unauthorised(String),
    /// The harness itself could not do what it was asked (a reply it was
    /// asked to record could not be written). The run stops.
    Failed(String),
}

impl Held {
    /// The error the client wraps in `ApiError::NotSent`. A dry run says
    /// exactly `dry run`.
    pub(crate) fn into_error(self) -> anyhow::Error {
        match self {
            Held::DryRun => anyhow::anyhow!("dry run"),
            Held::Halted(file) => anyhow::anyhow!("halted: the halt file {file} is present"),
            Held::Unauthorised(why) => anyhow::anyhow!("not authorised: {why}"),
            Held::Failed(why) => anyhow::anyhow!("the harness failed: {why}"),
        }
    }
}

/// What the client consults. Both methods run on the thread that makes the
/// call and must not block.
pub(crate) trait Wire: Send + Sync {
    /// Called before a call is signed or sent. `Err` means it is not sent.
    fn permit(&self, call: &Call) -> Result<(), Held>;

    /// Called with each HTTP reply, before it is parsed: the status and the
    /// body exactly as the venue sent them.
    fn observed(&self, method: &str, path: &str, status: u16, body: &str);
}
