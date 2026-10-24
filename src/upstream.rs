//! Validation for a single, explicitly selected public Responses endpoint.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Result, bail};
use reqwest::Url;

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(matches!(a, 0 | 10 | 127)
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && matches!(c, 0 | 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    let words = ip.segments();
    // Only global unicast allocation, excluding protocol assignments,
    // documentation networks and the deprecated 6to4 transition range.
    (words[0] & 0xe000) == 0x2000
        && !(words[0] == 0x2001 && (words[1] < 0x0200 || words[1] == 0x0db8))
        && words[0] != 0x2002
        && !(words[0] == 0x3fff && words[1] < 0x1000)
}

fn public_domain(host: &str) -> bool {
    let local_suffixes = [
        "localhost",
        "local",
        "localdomain",
        "lan",
        "internal",
        "intranet",
        "home",
        "home.arpa",
        "onion",
        "test",
        "invalid",
    ];
    host.contains('.')
        && !local_suffixes.iter().any(|suffix| {
            host == *suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

/// Normalize HTTPS root and `/v1` URLs to a single public endpoint.
/// Errors deliberately omit the supplied value because it may hold a secret.
/// This validates URL syntax and literal hosts; it does not perform DNS lookup.
pub fn normalize_base_url(value: &str) -> Result<String> {
    let invalid = || {
        anyhow::anyhow!(
            "上游地址须为公网 HTTPS 主机的根路径或 /v1，且不得包含认证信息、查询参数或片段"
        )
    };
    if value.chars().any(char::is_control) {
        return Err(invalid());
    }
    let value = value.trim();
    let mut url = Url::parse(value).map_err(|_| invalid())?;
    // Inspect the authority as well: the URL parser drops an empty `@`.
    let raw = value.split_once("://").ok_or_else(invalid)?.1;
    let authority = raw.split(['/', '?', '#']).next().unwrap_or_default();
    let raw_path = &raw[authority.len()..];
    if url.scheme() != "https"
        || authority.contains('@')
        || value.contains('\\')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(raw_path, "" | "/" | "/v1" | "/v1/")
    {
        return Err(invalid());
    }
    let raw_host = url.host_str().ok_or_else(invalid)?;
    // A single root-label dot is valid DNS notation; repeated trailing dots
    // contain empty labels and must not silently become an accepted host.
    let host = raw_host.strip_suffix('.').unwrap_or(raw_host).to_owned();
    let public = if host.starts_with('[') && host.ends_with(']') {
        host[1..host.len() - 1]
            .parse::<Ipv6Addr>()
            .is_ok_and(public_ipv6)
    } else if let Ok(ip) = host.parse::<Ipv4Addr>() {
        public_ipv4(ip)
    } else {
        public_domain(&host)
    };
    if !public {
        bail!("上游地址必须使用公网主机，不能使用本机、私有网络或保留地址");
    }
    url.set_host(Some(&host)).map_err(|_| invalid())?;
    url.set_path("/v1");
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_https_roots_default_port_and_trailing_slashes() {
        for value in [
            "https://api.example.com",
            "https://api.example.com/",
            "https://API.EXAMPLE.COM/v1",
            "https://api.example.com/v1/",
            "https://api.example.com:443/v1",
            " https://api.example.com./v1 ",
        ] {
            assert_eq!(
                normalize_base_url(value).unwrap(),
                "https://api.example.com/v1"
            );
        }
        assert_eq!(
            normalize_base_url("https://8.8.8.8").unwrap(),
            "https://8.8.8.8/v1"
        );
        assert_eq!(
            normalize_base_url("https://[2606:4700:4700::1111]").unwrap(),
            "https://[2606:4700:4700::1111]/v1"
        );
    }

    #[test]
    fn rejects_non_public_hosts_and_alternate_literal_ip_notation() {
        for host in [
            "localhost",
            "LOCALHOST.",
            "service.local",
            "api.localhost",
            "api.home.arpa",
            "api.localdomain",
            "api.internal",
            "api.lan",
            "api.intranet",
            "api.home",
            "api.invalid",
            "api.test",
            "singlelabel",
            "api.example.com..",
            "api..example.com",
            "0.0.0.0",
            "127.0.0.1",
            "127.1",
            "2130706433",
            "0x7f000001",
            "10.0.0.1",
            "172.16.1.2",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.0.8",
            "192.0.2.1",
            "198.19.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "[::1]",
            "[::]",
            "[::ffff:127.0.0.1]",
            "[fc00::1]",
            "[fe80::1]",
            "[ff02::1]",
            "[2001:db8::1]",
            "[2001::1]",
            "[2002:7f00:1::]",
            "[3fff::1]",
        ] {
            assert!(
                normalize_base_url(&format!("https://{host}/v1")).is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn rejects_authentication_and_non_endpoint_urls_without_echoing_input() {
        for value in [
            "http://api.example.com/v1",
            "https://user:sk-secret@api.example.com/v1",
            "https://@api.example.com/v1",
            "https://api.example.com:444/v1",
            "https://api.example.com/v1?q=sk-secret",
            "https://api.example.com/v1#sk-secret",
            "https://api.example.com/other",
            "https://api.example.com/other/../v1",
            "https://api.example.com/%76%31",
            "https://api.exam\nple.com/v1",
            "https://api.example.com\\v1",
            "not-a-url-sk-secret",
        ] {
            let error = normalize_base_url(value).unwrap_err().to_string();
            assert!(!error.contains(value));
            assert!(!error.contains("sk-secret"));
        }
    }
}
