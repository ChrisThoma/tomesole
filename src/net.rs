//! Hardened HTTP layer.
//!
//! Libgen is a moving target served by third-party mirrors, so every request
//! this tool makes is treated as talking to an untrusted host:
//!
//! * TLS certificates are always verified. There is deliberately no flag to
//!   turn that off.
//! * Only `http`/`https` are followed; `file:`, `ftp:` and friends are refused.
//! * Cleartext `http` is refused unless the user explicitly opts in.
//! * Redirects are **followed manually**, one hop at a time, so every hop is
//!   re-validated. A mirror cannot bounce us onto `localhost`, a link-local
//!   address, or a cloud metadata endpoint.
//! * Hostnames are resolved and every resulting address is checked against
//!   private and reserved ranges before we connect.
//!
//! URLs are parsed with `ureq`'s own re-exported `http::Uri` type rather than a
//! separate URL crate, so the address this module validates is parsed by
//! exactly the same code that later opens the socket. A second parser would
//! reintroduce the parsing-differential bug that the guard exists to prevent.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use ureq::http::Uri;

use crate::error::{Context, Error, Result};
use crate::{bail, err};

/// Sent on every request. Deliberately ordinary: we are not impersonating a
/// specific browser, only avoiding a default agent string that some mirrors
/// reject outright.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) tomesole/0.1";

/// Cap on HTML pages we parse, so a hostile mirror cannot exhaust memory.
pub const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Policy knobs the CLI can flip.
#[derive(Debug, Clone, Copy)]
pub struct NetPolicy {
    /// Allow cleartext `http://` URLs. Off by default.
    pub allow_http: bool,
    /// Allow hosts that resolve to private/loopback addresses. Off by default;
    /// only useful when pointing the tool at a mirror on a local network.
    pub allow_private_hosts: bool,
    pub connect_timeout: Duration,
    /// Whole-request deadline for small requests (searches, link resolution).
    pub request_timeout: Duration,
    pub max_redirects: usize,
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self {
            allow_http: false,
            allow_private_hosts: false,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_redirects: 8,
        }
    }
}

/// Reject URIs whose *shape* is unacceptable, before any DNS or TCP work.
pub fn check_uri_shape(uri: &Uri, policy: &NetPolicy) -> Result<()> {
    let scheme = uri.scheme_str().unwrap_or_default();
    match scheme {
        "https" => {}
        "http" => {
            if !policy.allow_http {
                bail!("refusing cleartext http for {uri} (pass --allow-http to permit it)");
            }
        }
        "" => bail!("URL has no scheme: {uri}"),
        other => bail!("refusing non-http(s) URL scheme `{other}` in {uri}"),
    }

    let authority = uri
        .authority()
        .ok_or_else(|| err!("URL has no host component: {uri}"))?;

    // `http::Uri` keeps userinfo in the authority; credentials in a mirror URL
    // are a redirect-laundering trick, not something we ever want to send.
    if authority.as_str().contains('@') {
        bail!("refusing URL with embedded credentials: {uri}");
    }

    let host = host_of(uri)?;
    if policy.allow_private_hosts {
        return Ok(());
    }

    // Bare IP literals never get a DNS lookup, so check them here.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            bail!("refusing to connect to non-public address {ip}");
        }
        return Ok(());
    }
    if is_forbidden_hostname(&host) {
        bail!("refusing to connect to local-only hostname `{host}`");
    }
    Ok(())
}

/// The host, with IPv6 brackets stripped so it parses as an [`IpAddr`].
pub fn host_of(uri: &Uri) -> Result<String> {
    let host = uri
        .host()
        .ok_or_else(|| err!("URL has no host component: {uri}"))?;
    Ok(host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase())
}

/// Resolve the host and ensure nothing it points at is private or reserved.
///
/// DNS can hand the connector a different answer than it handed us, so this is
/// defence in depth rather than an airtight guarantee. It still closes the
/// obvious "mirror redirects you at 169.254.169.254" hole.
pub fn check_uri_resolves_publicly(uri: &Uri, policy: &NetPolicy) -> Result<()> {
    check_uri_shape(uri, policy)?;
    if policy.allow_private_hosts {
        return Ok(());
    }
    let host = host_of(uri)?;
    if host.parse::<IpAddr>().is_ok() {
        return Ok(()); // literal, already checked
    }
    let port = uri.port_u16().unwrap_or(443);

    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve host `{host}`"))?;

    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        if is_forbidden_ip(addr.ip()) {
            bail!(
                "host `{host}` resolves to non-public address {} — refusing to connect",
                addr.ip()
            );
        }
    }
    if !saw_any {
        bail!("host `{host}` did not resolve to any address");
    }
    Ok(())
}

/// Hostnames that should never leave the machine.
fn is_forbidden_hostname(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" {
        return true;
    }
    const LOCAL_SUFFIXES: [&str; 6] = [
        ".localhost",
        ".local",
        ".internal",
        ".intranet",
        ".home.arpa",
        ".localdomain",
    ];
    LOCAL_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// True for any address we refuse to talk to: loopback, private, link-local,
/// carrier-grade NAT, multicast, and the various reserved blocks.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => {
            // Treat `::ffff:10.0.0.1` exactly like `10.0.0.1`.
            match v6.to_ipv4_mapped() {
                Some(mapped) => is_forbidden_v4(mapped),
                None => is_forbidden_v6(v6),
            }
        }
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0                                   // 0.0.0.0/8 "this network"
        || (a == 100 && (64..=127).contains(&b))    // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0)           // 192.0.0.0/24 protocol assignments
        || (a == 198 && (b == 18 || b == 19))       // 198.18.0.0/15 benchmarking
        || a >= 240 // 240.0.0.0/4 reserved
}

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00            // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80            // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
}

/// A validated HTTP client.
pub struct Http {
    agent: ureq::Agent,
    policy: NetPolicy,
}

/// The outcome of a single validated request.
pub struct Fetched {
    pub status: u16,
    pub body: ureq::Body,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub content_disposition: Option<String>,
    pub content_range: Option<String>,
}

impl Http {
    /// Build a client. Infallible: configuring an agent touches no resource
    /// that could refuse, and nothing is opened until the first request.
    pub fn new(policy: NetPolicy) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(policy.connect_timeout))
            // We follow redirects ourselves so each hop can be re-validated.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .https_only(!policy.allow_http)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            policy,
        }
    }

    /// Issue a GET, following redirects manually and validating every hop.
    ///
    /// `range` optionally requests a byte range, used to resume downloads.
    pub fn get(&self, uri: &Uri, range: Option<u64>) -> Result<Fetched> {
        self.request(uri, range, None, true)
    }

    /// Issue a GET whose body will be streamed to disk.
    ///
    /// The ordinary request deadline is deliberately limited to small page
    /// requests. A healthy large transfer may take much longer, so downloads
    /// retain the connect bound but have no whole-body deadline.
    pub fn get_download(&self, uri: &Uri, range: Option<u64>) -> Result<Fetched> {
        self.request(uri, range, None, false)
    }

    /// A GET that says which page it came from.
    ///
    /// Needed for cover art: the mirrors serve an empty file to anyone asking
    /// for an image without a `Referer`, which is ordinary hotlink protection
    /// and not something to route around — we really did come from that page.
    ///
    /// The header is only sent to the host it names, so a redirect elsewhere
    /// cannot be used to learn which record was being looked at.
    pub fn get_referred(&self, uri: &Uri, referer: &Uri) -> Result<Fetched> {
        self.request(uri, None, Some(referer), true)
    }

    fn request(
        &self,
        uri: &Uri,
        range: Option<u64>,
        referer: Option<&Uri>,
        bounded_body: bool,
    ) -> Result<Fetched> {
        let mut current = uri.clone();
        let mut hops = 0usize;

        loop {
            check_uri_resolves_publicly(&current, &self.policy)?;

            let config = self.agent.get(current.to_string()).config();
            let mut req = if bounded_body {
                config
                    .timeout_global(Some(self.policy.request_timeout))
                    .build()
            } else {
                config
                    .timeout_global(None)
                    // In ureq the response timeout remains an ancestor of the
                    // body-read timer, so setting it would still cap the whole
                    // transfer. Connection establishment remains bounded by
                    // the agent policy.
                    .timeout_recv_response(None)
                    .timeout_recv_body(None)
                    .build()
            };

            if let Some(from) = range {
                req = req.header("Range", format!("bytes={from}-"));
            }
            if let Some(referer) = referer
                && let (Ok(a), Ok(b)) = (host_of(referer), host_of(&current))
                && a.eq_ignore_ascii_case(&b)
            {
                req = req.header("Referer", referer.to_string());
            }
            // Ask for identity when ranging: a re-encoded body would break the
            // byte offsets we are resuming from.
            let resp = if range.is_some() {
                req.header("Accept-Encoding", "identity").call()
            } else {
                req.call()
            };

            // Report the host, not the whole URL: these messages end up in the
            // mirror status table, where a long query string hides the reason.
            let resp = resp.map_err(|e| {
                let host = host_of(&current).unwrap_or_else(|_| current.to_string());
                err!("{host}: {e}")
            })?;
            let status = resp.status().as_u16();

            if (300..400).contains(&status) {
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
                    .ok_or_else(|| err!("{current} returned HTTP {status} with no Location"))?;

                hops += 1;
                if hops > self.policy.max_redirects {
                    bail!(
                        "too many redirects (>{}) starting at {uri}",
                        self.policy.max_redirects
                    );
                }
                let next = join_uri(&current, &location)
                    .with_context(|| format!("bad redirect target `{location}` from {current}"))?;
                // Validated at the top of the next iteration.
                current = next;
                continue;
            }

            let headers = resp.headers();
            let content_type = header_string(headers, "content-type");
            let content_disposition = header_string(headers, "content-disposition");
            let content_range = header_string(headers, "content-range");
            let body = resp.into_body();
            let content_length = body.content_length();

            return Ok(Fetched {
                status,
                body,
                content_type,
                content_length,
                content_disposition,
                content_range,
            });
        }
    }

    /// GET a page and read it as text, with a hard size cap.
    ///
    /// Decoding is lossy on purpose: a few mirrors serve mislabelled encodings
    /// and we would rather parse a slightly mangled title than fail outright.
    pub fn get_text(&self, uri: &Uri) -> Result<String> {
        let fetched = self.get(uri, None)?;
        if !(200..300).contains(&fetched.status) {
            bail!("{uri} returned HTTP {}", fetched.status);
        }
        fetched
            .body
            .into_with_config()
            .limit(MAX_PAGE_BYTES)
            .lossy_utf8(true)
            .read_to_string()
            .with_context(|| format!("could not read response body from {uri}"))
    }
}

fn header_string(headers: &ureq::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Resolve `reference` against `base`, the way a browser would for the subset
/// of forms Libgen actually emits (absolute, protocol-relative, rooted, and
/// plain relative paths).
pub fn join_uri(base: &Uri, reference: &str) -> Result<Uri> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("empty URL reference");
    }
    // Drop any fragment; it is never sent to the server.
    let reference = reference.split('#').next().unwrap_or(reference);

    let scheme = base.scheme_str().unwrap_or("https");
    let authority = base
        .authority()
        .ok_or_else(|| err!("base URL has no authority: {base}"))?
        .as_str();

    let joined = if has_scheme(reference) {
        reference.to_string()
    } else if let Some(rest) = reference.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if reference.starts_with('/') {
        format!("{scheme}://{authority}{reference}")
    } else {
        // Relative to the base's directory.
        let base_path = base.path();
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{scheme}://{authority}{dir}{reference}")
    };

    let uri: Uri = normalize_dot_segments(&joined)
        .parse()
        .map_err(|e| Error::from(format!("invalid URL `{joined}`: {e}")))?;
    Ok(uri)
}

/// Whether a reference names its own scheme, as in `https://host/path`.
///
/// Testing for a bare `://` anywhere in the string is not good enough: Libgen
/// and its interstitials routinely emit links that carry a URL inside a query
/// parameter — `/out.php?u=https://cdn.example/f` — and reading one of those as
/// absolute drops the base and yields a host-less URI that fails validation
/// several steps later with an error naming the wrong thing. The separator has
/// to come before anything that would have started a path, query or fragment.
fn has_scheme(reference: &str) -> bool {
    let Some(colon) = reference.find("://") else {
        return false;
    };
    let scheme = &reference[..colon];
    !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Collapse `.` and `..` segments so a relative link cannot climb out of the
/// mirror's path space in a way the guard would not expect.
fn normalize_dot_segments(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = scheme_end + 3;
    let rest = &url[after_scheme..];
    // Split authority from path+query.
    let path_start = rest.find('/').unwrap_or(rest.len());
    let (authority, path_and_query) = rest.split_at(path_start);
    if path_and_query.is_empty() {
        return url.to_string();
    }
    let (path, query) = match path_and_query.find('?') {
        Some(i) => path_and_query.split_at(i),
        None => (path_and_query, ""),
    };

    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut normalized = String::from(&url[..after_scheme]);
    normalized.push_str(authority);
    normalized.push('/');
    normalized.push_str(&out.join("/"));
    normalized.push_str(query);
    normalized
}

/// Percent-encode a string for use inside a query-string value.
///
/// Keeps the RFC 3986 unreserved set, encodes everything else. Spaces become
/// `+`, which is what the Libgen search form expects.
pub fn encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len() + 8);
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Run `op` up to `attempts` times with exponential backoff.
pub fn with_retry<T, F>(attempts: usize, mut op: F) -> Result<T>
where
    F: FnMut(usize) -> Result<T>,
{
    let mut delay = Duration::from_millis(400);
    let mut last_err = None;
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        match op(attempt) {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < attempts {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| err!("operation failed with no error recorded")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_loopback_and_private_addresses() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.5",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "255.255.255.255",
            "240.0.0.1",
            "192.0.0.1",
            "198.18.0.1",
        ] {
            assert!(is_forbidden_ip(s.parse().unwrap()), "{s} should be blocked");
        }
    }

    #[test]
    fn blocks_ipv6_local_ranges() {
        for s in [
            "::1",
            "fe80::1",
            "fd00::1",
            "::",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(is_forbidden_ip(s.parse().unwrap()), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_ordinary_public_addresses() {
        for s in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(
                !is_forbidden_ip(s.parse().unwrap()),
                "{s} should be allowed"
            );
        }
    }

    #[test]
    fn rejects_dangerous_uri_shapes() {
        let p = NetPolicy::default();
        for bad in [
            "http://libgen.li/", // cleartext without opt-in
            "ftp://libgen.li/x",
            "https://user:pw@libgen.li/", // embedded credentials
            "https://localhost/x",
            "https://router.local/x",
            "https://foo.internal/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest/meta-data/",
        ] {
            assert!(
                check_uri_shape(&uri(bad), &p).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_normal_mirror_uris() {
        let p = NetPolicy::default();
        assert!(check_uri_shape(&uri("https://libgen.li/index.php?req=rust"), &p).is_ok());
    }

    #[test]
    fn http_allowed_only_with_opt_in() {
        let u = uri("http://libgen.li/");
        assert!(check_uri_shape(&u, &NetPolicy::default()).is_err());
        let relaxed = NetPolicy {
            allow_http: true,
            ..NetPolicy::default()
        };
        assert!(check_uri_shape(&u, &relaxed).is_ok());
    }

    #[test]
    fn joins_relative_links_like_a_browser() {
        let base = uri("https://libgen.li/ads.php?md5=abc");
        assert_eq!(
            join_uri(&base, "get.php?md5=abc&key=K")
                .unwrap()
                .to_string(),
            "https://libgen.li/get.php?md5=abc&key=K"
        );
        assert_eq!(
            join_uri(&base, "/file.php?id=1").unwrap().to_string(),
            "https://libgen.li/file.php?id=1"
        );
        assert_eq!(
            join_uri(&base, "https://other.example/x")
                .unwrap()
                .to_string(),
            "https://other.example/x"
        );
        assert_eq!(
            join_uri(&base, "//cdn.example/x").unwrap().to_string(),
            "https://cdn.example/x"
        );
    }

    #[test]
    fn join_resolves_dot_segments_and_drops_fragments() {
        let base = uri("https://libgen.li/a/b/page.php");
        assert_eq!(
            join_uri(&base, "../c/get.php").unwrap().to_string(),
            "https://libgen.li/a/c/get.php"
        );
        assert_eq!(
            join_uri(&base, "get.php#frag").unwrap().to_string(),
            "https://libgen.li/a/b/get.php"
        );
    }

    /// Interstitials and redirects routinely carry a URL inside a query
    /// parameter. Reading the `://` in one as a scheme dropped the base and
    /// produced a host-less URI that failed validation much later on.
    #[test]
    fn a_url_inside_a_query_string_does_not_look_absolute() {
        let base = uri("https://libgen.li/a/b/page.php");
        assert_eq!(
            join_uri(&base, "/out.php?u=https://cdn.example/f")
                .unwrap()
                .to_string(),
            "https://libgen.li/out.php?u=https://cdn.example/f"
        );
        assert_eq!(
            join_uri(&base, "get.php?ref=http://x.example/y")
                .unwrap()
                .to_string(),
            "https://libgen.li/a/b/get.php?ref=http://x.example/y"
        );
        // A protocol-relative reference carrying one stays protocol-relative.
        assert_eq!(
            join_uri(&base, "//cdn.example/f?to=https://z.example/q")
                .unwrap()
                .to_string(),
            "https://cdn.example/f?to=https://z.example/q"
        );
    }

    #[test]
    fn a_genuine_scheme_is_still_recognised() {
        assert!(has_scheme("https://libgen.li/x"));
        assert!(has_scheme("HTTP://libgen.li/x"));
        assert!(has_scheme("ftp://libgen.li/x"));
        assert!(!has_scheme("/out.php?u=https://x"));
        assert!(!has_scheme("get.php?ref=http://x"));
        assert!(!has_scheme("//cdn.example/x"));
        assert!(!has_scheme("://nothing"));
        assert!(!has_scheme("9x://digits-first"));
        assert!(!has_scheme("plain/path"));
    }

    /// A non-http scheme smuggled through a query must still be refused when
    /// it is the reference proper.
    #[test]
    fn a_joined_non_http_scheme_is_refused() {
        let base = uri("https://libgen.li/ads.php");
        let policy = NetPolicy::default();
        for hostile in ["file:///etc/passwd", "ftp://libgen.li/x", "gopher://x/1"] {
            // Refused either at the join, when `http::Uri` will not hold it, or
            // at the shape check. Which one does not matter; both are a stop.
            let refused = match join_uri(&base, hostile) {
                Ok(joined) => check_uri_shape(&joined, &policy).is_err(),
                Err(_) => true,
            };
            assert!(refused, "{hostile} should not be reachable");
        }
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query_value("rust programming"), "rust+programming");
        assert_eq!(encode_query_value("c++ & co"), "c%2B%2B+%26+co");
        assert_eq!(encode_query_value("naïve"), "na%C3%AFve");
        assert_eq!(encode_query_value("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn retry_gives_up_and_reports_last_error() {
        let r: Result<()> = with_retry(2, |_| Err(err!("nope")));
        assert_eq!(r.unwrap_err().to_string(), "nope");
    }

    #[test]
    fn retry_succeeds_after_failure() {
        let v = with_retry(3, |attempt| {
            if attempt == 0 {
                Err(err!("transient"))
            } else {
                Ok(attempt)
            }
        })
        .unwrap();
        assert_eq!(v, 1);
    }

    // --- The redirect loop, exercised against a throwaway local server. ---
    //
    // Following redirects by hand is the whole reason every hop can be
    // re-validated, so the loop is tested through `Http` itself rather than by
    // unit-testing the pieces it calls.

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// One request, as the test server saw it on the wire.
    #[derive(Clone)]
    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        /// The request target, including its query string.
        fn path(&self) -> &str {
            self.line.split(' ').nth(1).unwrap_or("")
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// A loopback server that answers a scripted sequence of raw responses and
    /// records what it was asked for. The last response repeats, so a
    /// self-referential redirect can be served for as long as a client keeps
    /// following it.
    struct Server {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Server {
        fn start(responses: Vec<Vec<u8>>) -> Self {
            assert!(!responses.is_empty(), "need at least one response");
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let recorder = Arc::clone(&seen);
            std::thread::spawn(move || {
                for (i, conn) in listener.incoming().enumerate() {
                    let Ok(mut stream) = conn else { return };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let mut lines: Vec<String> = Vec::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        lines.push(line.trim_end().to_string());
                        line.clear();
                    }
                    let Some(request_line) = lines.first().cloned() else {
                        continue;
                    };
                    let headers = lines[1..]
                        .iter()
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .collect();
                    recorder.lock().unwrap().push(Seen {
                        line: request_line,
                        headers,
                    });
                    let response = &responses[i.min(responses.len() - 1)];
                    let _ = stream.write_all(response);
                    let _ = stream.flush();
                }
            });
            Self { port, seen }
        }

        fn url(&self, path: &str) -> Uri {
            format!("http://127.0.0.1:{}{path}", self.port)
                .parse()
                .unwrap()
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn redirect_to(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    fn page(body: &str) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body.as_bytes());
        out
    }

    /// Loopback is refused by default, so these tests opt in the way
    /// `--allow-private-hosts` does.
    fn loopback_http(max_redirects: usize) -> Http {
        Http::new(NetPolicy {
            allow_http: true,
            allow_private_hosts: true,
            max_redirects,
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        })
    }

    #[test]
    fn follows_redirects_relative_to_the_hop_that_sent_them() {
        let server = Server::start(vec![
            redirect_to("get.php?x=1"), // relative to the current directory
            redirect_to("/final"),      // rooted at the current host
            page("done"),
        ]);
        let body = loopback_http(8)
            .get_text(&server.url("/a/b/page.php"))
            .unwrap();
        assert_eq!(body, "done");

        let seen = server.seen();
        let paths: Vec<&str> = seen.iter().map(Seen::path).collect();
        assert_eq!(paths, ["/a/b/page.php", "/a/b/get.php?x=1", "/final"]);
    }

    /// Interstitials bounce through links that carry a URL inside a query
    /// parameter. Reading the `://` in one as a scheme dropped the base and
    /// left a host-less URI that failed several steps later, naming the wrong
    /// thing. Exercised here through the caller that hit it.
    #[test]
    fn follows_a_redirect_carrying_a_url_in_its_query() {
        let server = Server::start(vec![
            redirect_to("/out.php?u=https://cdn.example/f"),
            page("done"),
        ]);
        let body = loopback_http(8)
            .get_text(&server.url("/ads.php?md5=abc"))
            .unwrap();
        assert_eq!(body, "done");

        let seen = server.seen();
        assert_eq!(seen.len(), 2, "the second hop should have been requested");
        assert_eq!(seen[1].path(), "/out.php?u=https://cdn.example/f");
    }

    /// A redirect target is validated before it is dialled, not after.
    ///
    /// The private-address half of that guard cannot be shown against a
    /// loopback server — reaching one at all takes the flag that switches the
    /// address check off — so the address rules are covered by
    /// [`the_hop_guard_refuses_cloud_metadata`] against the real policy, and
    /// what is shown here is that the guard runs on a hop rather than only on
    /// the URL the caller supplied.
    #[test]
    fn a_hostile_redirect_target_is_refused_mid_chain() {
        for hostile in [
            "ftp://libgen.li/x",
            "file:///etc/passwd",
            "gopher://libgen.li/1",
            "https://user:pw@libgen.li/",
        ] {
            let server = Server::start(vec![redirect_to(hostile), page("should never be served")]);
            let err = loopback_http(8)
                .get_text(&server.url("/ads.php"))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("refusing") || err.contains("bad redirect target"),
                "{hostile} gave: {err}"
            );
            assert_eq!(
                server.seen().len(),
                1,
                "{hostile} should stop the chain, not be followed"
            );
        }
    }

    /// The check each hop runs, on the addresses a loopback test cannot use.
    #[test]
    fn the_hop_guard_refuses_cloud_metadata() {
        let policy = NetPolicy::default();
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/",
            "https://127.0.0.1/x",
            "https://localhost/x",
        ] {
            assert!(
                check_uri_resolves_publicly(&uri(bad), &policy).is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn caps_the_hop_count_and_names_the_original_url() {
        let server = Server::start(vec![redirect_to("/loop")]);
        let start = server.url("/first.php");
        let err = loopback_http(2)
            .get_text(&start)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too many redirects (>2)"), "got: {err}");
        assert!(err.contains(&start.to_string()), "got: {err}");
        assert_eq!(server.seen().len(), 3, "one request per allowed hop");
    }

    #[test]
    fn a_redirect_without_a_location_is_an_error() {
        let server = Server::start(vec![
            b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let err = loopback_http(8)
            .get_text(&server.url("/x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("with no Location"), "got: {err}");
    }

    /// The mirrors serve an empty image to anyone asking without a `Referer`,
    /// so we send one — but only to the host it names, or a redirect elsewhere
    /// would leak which record was being looked at.
    #[test]
    fn a_referer_is_sent_only_to_the_host_it_names() {
        let server = Server::start(vec![page("image bytes")]);
        let referer = server.url("/file.php?id=1");
        loopback_http(8)
            .get_referred(&server.url("/covers/x.jpg"), &referer)
            .unwrap();
        assert_eq!(
            server.seen()[0].header("referer"),
            Some(referer.to_string().as_str())
        );

        // Same address, different name: the header must not travel.
        let other = Server::start(vec![page("image bytes")]);
        let elsewhere: Uri = format!("http://localhost:{}/file.php?id=1", other.port)
            .parse()
            .unwrap();
        loopback_http(8)
            .get_referred(&other.url("/covers/x.jpg"), &elsewhere)
            .unwrap();
        assert_eq!(other.seen()[0].header("referer"), None);
    }

    /// Resuming means appending at a byte offset, so a re-encoded body would
    /// corrupt the file: a ranged request has to ask for identity.
    #[test]
    fn a_ranged_request_asks_for_an_untransformed_body() {
        let server = Server::start(vec![
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 10-11/12\r\n\
              Content-Length: 2\r\nConnection: close\r\n\r\nok"
                .to_vec(),
        ]);
        let fetched = loopback_http(8).get(&server.url("/f"), Some(10)).unwrap();
        assert_eq!(fetched.status, 206);
        assert_eq!(fetched.content_range.as_deref(), Some("bytes 10-11/12"));

        let seen = server.seen();
        assert_eq!(seen[0].header("range"), Some("bytes=10-"));
        assert_eq!(seen[0].header("accept-encoding"), Some("identity"));
    }

    #[test]
    fn get_text_enforces_the_page_cap() {
        let oversized = "x".repeat(MAX_PAGE_BYTES as usize + 1);
        let server = Server::start(vec![page(&oversized)]);
        let err = loopback_http(8)
            .get_text(&server.url("/huge"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not read response body"), "got: {err}");
    }

    /// A failing status never reaches `get_text`'s own check — ureq turns 4xx
    /// and 5xx into an error of its own — so what matters is that the message
    /// still names the host it came from, which is all the mirror status table
    /// has room to show.
    #[test]
    fn a_failing_status_is_reported_against_its_host() {
        let server = Server::start(vec![
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let err = loopback_http(8)
            .get_text(&server.url("/gone"))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("127.0.0.1:"), "got: {err}");
        assert!(err.contains("404"), "got: {err}");
    }
}
