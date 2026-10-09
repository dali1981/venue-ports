//! The host guards: a production run goes to production, and to nothing that
//! only looks like it.
//!
//! The Binance guard takes the allowed host as a parameter so that a unit test
//! can use a mock server; the only value an ignored test passes is
//! [`PRODUCTION_BINANCE_SPOT`]. The testnet, any other host and an unset
//! variable are each refused by name.

use std::fmt;

/// The one host the Binance spot production tier runs against, exactly.
pub(crate) const PRODUCTION_BINANCE_SPOT: &str = "https://api.binance.com";

/// Why a host or a node was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The variable that names the host is not set. The crate's own default
    /// for `BINANCE_BASE_URL` is the testnet, so this tier never reads it with
    /// that default.
    Unset(&'static str),
    /// The Binance spot testnet, by any spelling that names it.
    Testnet(String),
    /// Some other host than the allowed one.
    NotAllowed { got: String, allowed: String },
    /// A node URL that is not a URL.
    NotAUrl(String),
    /// A node on this machine.
    Local(String),
    /// A node that reports itself as anvil.
    Anvil(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Unset(var) => write!(
                f,
                "{var} is not set: the production tier names its host and never assumes one \
                 (the crate's default is the testnet)"
            ),
            Refused::Testnet(url) => write!(
                f,
                "refusing {url}: that is the Spot testnet, and this tier runs against production only"
            ),
            Refused::NotAllowed { got, allowed } => write!(
                f,
                "refusing {got}: this tier runs against exactly {allowed}"
            ),
            Refused::NotAUrl(url) => write!(f, "refusing {url:?}: it is not a URL"),
            Refused::Local(host) => write!(
                f,
                "refusing the node at {host}: a production node is not on this machine"
            ),
            Refused::Anvil(version) => write!(
                f,
                "refusing the node, which reports itself as {version}: anvil is a fork, not a chain"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// Whether `configured` (the value of `var`) is the host this tier may use:
/// exactly `allowed`.
pub(crate) fn binance_host(
    var: &'static str,
    configured: Option<&str>,
    allowed: &str,
) -> Result<(), Refused> {
    let url = configured
        .filter(|url| !url.is_empty())
        .ok_or(Refused::Unset(var))?;
    if url == allowed {
        return Ok(());
    }
    if url.to_ascii_lowercase().contains("testnet") {
        return Err(Refused::Testnet(url.to_string()));
    }
    Err(Refused::NotAllowed {
        got: url.to_string(),
        allowed: allowed.to_string(),
    })
}

/// Whether `url` may be a production node's: a URL, and not this machine.
pub(crate) fn node_url(url: &str) -> Result<(), Refused> {
    let parsed = reqwest::Url::parse(url).map_err(|_| Refused::NotAUrl(url.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Refused::NotAUrl(url.to_string()))?
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    let on_this_machine = host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host == "::"
        || host == "0.0.0.0"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    if on_this_machine {
        return Err(Refused::Local(host));
    }
    Ok(())
}

/// Whether a node whose `web3_clientVersion` is `version` may be a production
/// node: not anvil.
pub(crate) fn node_version(version: &str) -> Result<(), Refused> {
    if version
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("anvil")
    {
        return Err(Refused::Anvil(version.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAR: &str = "BINANCE_BASE_URL";

    #[test]
    fn only_the_allowed_host_passes_exactly() {
        assert_eq!(
            binance_host(VAR, Some(PRODUCTION_BINANCE_SPOT), PRODUCTION_BINANCE_SPOT),
            Ok(())
        );
        // A mock server, when the test passes its address as the allowed host.
        assert_eq!(
            binance_host(VAR, Some("http://127.0.0.1:4040"), "http://127.0.0.1:4040"),
            Ok(())
        );
    }

    #[test]
    fn the_testnet_is_refused_by_name() {
        let err = binance_host(
            VAR,
            Some("https://testnet.binance.vision"),
            PRODUCTION_BINANCE_SPOT,
        )
        .unwrap_err();
        assert!(matches!(err, Refused::Testnet(_)), "{err}");
        assert!(err.to_string().contains("Spot testnet"), "{err}");
    }

    #[test]
    fn a_mock_host_is_refused_when_production_is_the_allowed_one() {
        let err =
            binance_host(VAR, Some("http://127.0.0.1:4040"), PRODUCTION_BINANCE_SPOT).unwrap_err();
        assert_eq!(
            err,
            Refused::NotAllowed {
                got: "http://127.0.0.1:4040".to_string(),
                allowed: PRODUCTION_BINANCE_SPOT.to_string(),
            }
        );
    }

    #[test]
    fn an_unset_or_empty_variable_is_refused_by_its_name() {
        for configured in [None, Some("")] {
            let err = binance_host(VAR, configured, PRODUCTION_BINANCE_SPOT).unwrap_err();
            assert_eq!(err, Refused::Unset(VAR));
            assert!(err.to_string().contains(VAR), "{err}");
        }
    }

    /// "Exactly": a near miss is not the host.
    #[test]
    fn a_near_miss_is_not_the_host() {
        for url in [
            "https://api.binance.com/",
            "http://api.binance.com",
            "https://api.binance.com:443",
            "https://api1.binance.com",
            "https://api.binance.com.evil.example",
        ] {
            assert!(
                matches!(
                    binance_host(VAR, Some(url), PRODUCTION_BINANCE_SPOT),
                    Err(Refused::NotAllowed { .. })
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn a_node_on_this_machine_is_refused() {
        for url in [
            "http://localhost:8545",
            "http://LOCALHOST:8545",
            "http://127.0.0.1:8545",
            "http://127.1.2.3:8545",
            "http://[::1]:8545",
            "http://0.0.0.0:8545",
            "http://anvil.localhost:8545",
        ] {
            assert!(matches!(node_url(url), Err(Refused::Local(_))), "{url}");
        }
        for url in [
            "https://mainnet.base.org",
            "https://base-mainnet.example.com/v2/abc",
            "http://10.0.0.5:8545",
        ] {
            assert_eq!(node_url(url), Ok(()), "{url}");
        }
        assert!(matches!(node_url("not a url"), Err(Refused::NotAUrl(_))));
    }

    #[test]
    fn an_anvil_is_refused_by_what_it_reports() {
        assert!(matches!(
            node_version("anvil/v1.3.0"),
            Err(Refused::Anvil(_))
        ));
        assert!(matches!(
            node_version("Anvil/v1.3.0"),
            Err(Refused::Anvil(_))
        ));
        assert_eq!(
            node_version("Geth/v1.14.0-stable/linux-amd64/go1.22"),
            Ok(())
        );
        assert_eq!(node_version("reth/v1.1.0"), Ok(()));
    }
}
