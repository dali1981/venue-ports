//! A node URL as an error or a `Debug` may name it: scheme, host and port. A provider's key is in its userinfo, path or query, and a
//! string that is not a URL is never echoed.

pub(crate) fn shown(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", u.scheme()),
            (Some(host), None) => format!("{}://{host}", u.scheme()),
            (None, _) => "<unparseable url>".to_string(),
        },
        Err(_) => "<unparseable url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::shown;

    #[test]
    fn a_node_is_shown_by_scheme_host_and_port() {
        assert_eq!(shown("https://user:pass@base.g.alchemy.com/v2/KEY?apikey=K2"), "https://base.g.alchemy.com");
        assert_eq!(shown("http://127.0.0.1:8545/KEY"), "http://127.0.0.1:8545");
        assert_eq!(shown("https://node.io:443/KEY"), "https://node.io", "the default port is not written");
        assert_eq!(shown("wss://node.io/ws/KEY"), "wss://node.io");
        assert_eq!(shown("node.io/v2/KEY"), "<unparseable url>", "not a URL: never echoed");
    }
}
