//! `client_id` / `redirect_uri` checks (`components/auth/indieauth.py`) and
//! HA's notion of a local address (`util/network.py`).
//!
//! URLs are split the way Python's `urllib.parse.urlparse` splits them, which
//! is what HA's checks see. HA's last-resort check, fetching the `client_id`
//! page for `<link rel="redirect_uri">` tags, is not done: the browser and the
//! mobile apps never need it, and `hars` makes no outbound request for it.

use std::net::IpAddr;

/// The parts of a URL these checks use, as `urlparse` returns them.
#[derive(Debug, PartialEq)]
struct Url<'a> {
    scheme: String,
    netloc: String,
    path: &'a str,
    fragment: &'a str,
}

fn urlparse(url: &str) -> Url<'_> {
    let (url, fragment) = url.split_once('#').unwrap_or((url, ""));
    let (url, _query) = url.split_once('?').unwrap_or((url, ""));
    let (scheme, rest) = match url.split_once(':') {
        Some((s, rest))
            if s.starts_with(|c: char| c.is_ascii_alphabetic())
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (s.to_ascii_lowercase(), rest)
        }
        _ => (String::new(), url),
    };
    let (netloc, path) = match rest.strip_prefix("//") {
        Some(rest) => {
            let end = rest.find('/').unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        }
        None => ("", rest),
    };
    // IndieAuth canonicalization: lowercase host, empty path means `/`.
    Url {
        scheme,
        netloc: netloc.to_lowercase(),
        path: if path.is_empty() { "/" } else { path },
        fragment,
    }
}

fn parse_client_id(client_id: &str) -> Option<Url<'_>> {
    let parts = urlparse(client_id);
    if !matches!(parts.scheme.as_str(), "http" | "https")
        || parts.path.split('/').any(|seg| seg == "." || seg == "..")
        || !parts.fragment.is_empty()
        || parts.netloc.contains('@')
    {
        return None;
    }
    // HA raises IndexError (a 500) on an empty host; reject it instead.
    if parts.netloc.is_empty() {
        return None;
    }
    // `ParseResult.port`: digits after the first `:` (after `]` for IPv6).
    let hostinfo = parts.netloc.as_str();
    let port = match hostinfo.strip_prefix('[') {
        Some(b) => b
            .split_once(']')
            .map(|(_, rest)| rest.split_once(':').map_or("", |p| p.1)),
        None => Some(hostinfo.split_once(':').map_or("", |p| p.1)),
    };
    match port {
        Some("") | None => {}
        Some(p) => {
            if !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u32>().ok()? > 65535 {
                return None;
            }
        }
    }
    // The netloc as a whole (port included) must be a local IP or not an IP
    // at all: `192.168.1.5:8123` is "not an IP", so it is accepted.
    let host = hostinfo
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(hostinfo);
    match host.parse::<IpAddr>() {
        Ok(ip) if !is_local(ip) => None,
        _ => Some(parts),
    }
}

/// `verify_client_id`.
pub fn verify_client_id(client_id: &str) -> bool {
    parse_client_id(client_id).is_some()
}

/// `verify_redirect_uri`, minus the outbound fetch: same scheme and host, or
/// one of the companion apps' fixed callbacks.
pub fn verify_redirect_uri(client_id: &str, redirect_uri: &str) -> bool {
    let Some(client) = parse_client_id(client_id) else {
        return false;
    };
    let redirect = urlparse(redirect_uri);
    if client.scheme == redirect.scheme && client.netloc == redirect.netloc {
        return true;
    }
    match client_id {
        "https://home-assistant.io/iOS" => redirect_uri == "homeassistant://auth-callback",
        "https://home-assistant.io/android" => matches!(
            redirect_uri,
            "homeassistant://auth-callback"
                | "https://wear.googleapis.com/3p_auth/io.homeassistant.companion.android"
                | "https://wear.googleapis-cn.com/3p_auth/io.homeassistant.companion.android"
        ),
        _ => false,
    }
}

/// `util.network.is_local`: loopback, private (RFC 6890) or link-local,
/// including IPv4-mapped IPv6 forms of the IPv4 ranges.
pub fn is_local(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first >> 8 == 0xfd || first & 0xffc0 == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids() {
        for ok in [
            "http://192.168.1.5:8123/",
            "http://homeassistant.local:8123/",
            "https://home-assistant.io/iOS",
            "https://home-assistant.io/android",
            "http://example.com",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://10.0.0.1/",
            "http://8.8.8.8:8123/", // not an IP once the port is included
            "http://host:/",        // empty port is no port
            "HTTP://Example.COM/",
        ] {
            assert!(verify_client_id(ok), "{ok}");
        }
        for bad in [
            "ftp://example.com/",
            "example.com",
            "http://example.com/./x",
            "http://example.com/a/../b",
            "http://example.com/#frag",
            "http://user@example.com/",
            "http://user:pw@example.com/",
            "http://example.com:abc/",
            "http://example.com:70000/",
            "http://8.8.8.8/",
            "http://[2001:4860::8888]/",
            "http:///x",
            "http:x",
            "",
        ] {
            assert!(!verify_client_id(bad), "{bad}");
        }
    }

    #[test]
    fn redirect_uris() {
        let browser = "http://192.168.1.5:8123/";
        assert!(verify_redirect_uri(
            browser,
            "http://192.168.1.5:8123/?auth_callback=1"
        ));
        assert!(verify_redirect_uri(
            browser,
            "http://192.168.1.5:8123/lovelace/0?auth_callback=1"
        ));
        assert!(verify_redirect_uri(
            "http://HOST:8123/",
            "http://host:8123/x"
        ));
        assert!(!verify_redirect_uri(browser, "https://192.168.1.5:8123/"));
        assert!(!verify_redirect_uri(browser, "http://192.168.1.5:8124/"));
        assert!(!verify_redirect_uri(browser, "http://evil.example/"));
        assert!(!verify_redirect_uri("http://8.8.8.8/", "http://8.8.8.8/"));

        let ios = "https://home-assistant.io/iOS";
        assert!(verify_redirect_uri(ios, "homeassistant://auth-callback"));
        assert!(!verify_redirect_uri(ios, "homeassistant://other"));
        let android = "https://home-assistant.io/android";
        assert!(verify_redirect_uri(
            android,
            "homeassistant://auth-callback"
        ));
        assert!(verify_redirect_uri(
            android,
            "https://wear.googleapis.com/3p_auth/io.homeassistant.companion.android"
        ));
        assert!(!verify_redirect_uri(
            android,
            "https://wear.googleapis.com/"
        ));
    }

    #[test]
    fn local_addresses() {
        for ip in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "169.254.1.1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.20",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_local(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "172.32.0.1",
            "2001:db8::1",
            "fc00::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_local(ip.parse().unwrap()), "{ip}");
        }
    }
}
