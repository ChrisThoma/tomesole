//! Finding a Libgen mirror that actually works.
//!
//! Libgen has no stable address. Domains get seized, expire, or start serving
//! an anti-bot challenge, and — the case that motivates most of this module —
//! a mirror will often serve a perfectly healthy front page while its search
//! endpoint returns HTTP 500. A liveness check that pings `/` would happily
//! route the user to one of those.
//!
//! So the health check here runs a real search and requires parseable results
//! back. A mirror is "up" only if it can do the thing we need it for.
//!
//! The strategy, in order:
//!
//! 1. Mirrors the user configured explicitly, tried first and never reordered.
//! 2. A cached ranking from a recent probe (cheap, avoids hammering mirrors).
//! 3. A concurrent probe of the built-in seed list, ranked by latency.
//! 4. Discovery: ask a working mirror for its own list of siblings, so a new
//!    domain can be picked up without a new release of this tool.
//!
//! Whatever comes out of that is a *pool*, not a single choice: if the chosen
//! mirror fails mid-operation, the next one is tried transparently.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ureq::http::Uri;

use crate::error::{Context, Result};
use crate::net::{Http, NetPolicy, check_uri_shape};
use crate::{bail, config, err, html, libgen};

/// Known Libgen front-ends, most reliable first.
///
/// This is only a starting point. Mirrors that have gone dark stay in the list
/// because they periodically come back, and probing them costs one concurrent
/// request that fails fast.
pub const SEED_MIRRORS: &[&str] = &[
    "https://libgen.li",
    "https://libgen.gl",
    "https://libgen.vg",
    "https://libgen.la",
    "https://libgen.bz",
    "https://libgen.gs",
    "https://libgen.is",
    "https://libgen.rs",
    "https://libgen.st",
];

/// How long a cached ranking stays valid.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Per-mirror deadline while probing. Short on purpose: a mirror that takes
/// longer than this to run a search is not one we want to download from.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct MirrorStatus {
    pub base: Uri,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Search worked and returned this many parseable rows.
    Healthy {
        latency: Duration,
        results: usize,
    },
    Unhealthy(String),
}

impl MirrorStatus {
    pub fn is_healthy(&self) -> bool {
        matches!(self.outcome, Outcome::Healthy { .. })
    }

    pub fn latency(&self) -> Option<Duration> {
        match self.outcome {
            Outcome::Healthy { latency, .. } => Some(latency),
            Outcome::Unhealthy(_) => None,
        }
    }
}

/// Normalise a user- or seed-supplied mirror string into a validated base URI.
pub fn parse_mirror(raw: &str, policy: &NetPolicy) -> Result<Uri> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("empty mirror address");
    }
    // Bare hostnames are a convenience; assume https.
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    // A trailing slash makes relative joins behave.
    let uri: Uri = format!("{with_scheme}/")
        .parse()
        .with_context(|| format!("`{raw}` is not a valid URL"))?;
    check_uri_shape(&uri, policy)?;
    Ok(uri)
}

/// Probe every candidate concurrently and return their statuses.
///
/// Threads rather than an async runtime: this is the only place in the program
/// that does anything concurrently, and it is a fixed, small fan-out.
pub fn probe_all(candidates: &[Uri], policy: &NetPolicy) -> Vec<MirrorStatus> {
    let probe_policy = NetPolicy {
        request_timeout: PROBE_TIMEOUT,
        connect_timeout: Duration::from_secs(6),
        ..*policy
    };

    std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|base| {
                scope.spawn(move || {
                    let started = Instant::now();
                    let outcome =
                        match libgen::probe(&Http::new(probe_policy), base) {
                            Ok(results) => Outcome::Healthy {
                                latency: started.elapsed(),
                                results,
                            },
                            Err(e) => Outcome::Unhealthy(short_reason(&e.to_string())),
                        };
                    MirrorStatus {
                        base: base.clone(),
                        outcome,
                    }
                })
            })
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// Trim a long error chain down to something that fits in a status table.
fn short_reason(message: &str) -> String {
    let first = message
        .split(&[':', '\n'][..])
        .next_back()
        .unwrap_or(message)
        .trim();
    let text = if first.len() < 8 { message } else { first };
    let text = text.trim();
    if text.len() > 60 {
        format!("{}…", &text[..text.floor_char_boundary(60)])
    } else {
        text.to_string()
    }
}

/// Order statuses: healthy mirrors by ascending latency, then the rest.
pub fn rank(statuses: &[MirrorStatus]) -> Vec<Uri> {
    let mut healthy: Vec<&MirrorStatus> = statuses.iter().filter(|s| s.is_healthy()).collect();
    healthy.sort_by_key(|s| s.latency().unwrap_or(Duration::MAX));
    healthy.iter().map(|s| s.base.clone()).collect()
}

/// Ask a working mirror which other mirrors it knows about.
///
/// Anything discovered this way is still probed and still ranked below the
/// seeds, and every download is MD5-verified regardless, so a mirror listing a
/// hostile sibling gains nothing. Names are filtered to plausible Libgen hosts
/// so this cannot be used to point the tool at arbitrary infrastructure.
pub fn discover(http: &Http, base: &Uri, policy: &NetPolicy) -> Vec<Uri> {
    let Ok(url) = crate::net::join_uri(base, "mirrors.php") else {
        return Vec::new();
    };
    let Ok(page) = http.get_text(&url) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for href in html::hrefs(&page) {
        if !href.starts_with("http") {
            continue;
        }
        let Ok(uri) = parse_mirror(&href, policy) else {
            continue;
        };
        let Ok(host) = crate::net::host_of(&uri) else {
            continue;
        };
        if !looks_like_libgen(&host) {
            continue;
        }
        let normalized = format!("https://{host}/");
        if let Ok(u) = normalized.parse::<Uri>()
            && !found.iter().any(|e: &Uri| e.to_string() == u.to_string())
        {
            found.push(u);
        }
    }
    found
}

/// Whether a discovered host is plausibly a Libgen front-end.
///
/// Matched against the registrable part of the name rather than the whole
/// string. A bare `contains` would accept `libgen.attacker.example`, where the
/// interesting half of the name is the part this check is meant to be reading.
fn looks_like_libgen(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    // `libgen.li` → `libgen`; `mirror.libgen.gs` → `libgen`. The last label is
    // the TLD, which moves constantly and says nothing.
    let mut labels = host.rsplit('.').skip(1);
    let Some(domain) = labels.next() else {
        return false;
    };
    // Step over a two-part suffix, so `libgen.co.uk` is read as `libgen` rather
    // than as `co`. Not a public-suffix list, just the handful that turn up.
    let domain = if matches!(domain, "co" | "com" | "net" | "org" | "ac" | "gov") {
        labels.next().unwrap_or(domain)
    } else {
        domain
    };
    domain.contains("libgen") || domain.contains("genesis")
}

/// An ordered set of mirrors to try, best first.
pub struct Pool {
    ordered: Vec<Uri>,
}

impl Pool {
    pub fn new(ordered: Vec<Uri>) -> Self {
        Self { ordered }
    }

    pub fn mirrors(&self) -> &[Uri] {
        &self.ordered
    }

    /// Run `op` against each mirror in turn until one succeeds.
    ///
    /// `on_retry` is called when a mirror fails and another is about to be
    /// tried, so the UI can say what happened instead of stalling silently.
    pub fn try_each<T, F, R>(&self, mut op: F, mut on_retry: R) -> Result<(T, Uri)>
    where
        F: FnMut(&Uri) -> Result<T>,
        R: FnMut(&Uri, &str),
    {
        if self.ordered.is_empty() {
            bail!("no usable Libgen mirror found");
        }
        let mut failures = Vec::new();

        for base in &self.ordered {
            match op(base) {
                Ok(value) => return Ok((value, base.clone())),
                Err(e) => {
                    let host = crate::net::host_of(base).unwrap_or_else(|_| base.to_string());
                    on_retry(base, &e.to_string());
                    failures.push(format!("  {host}: {}", short_reason(&e.to_string())));
                }
            }
        }

        Err(err!(
            "every mirror failed:\n{}\n\nTry `tomesole mirrors --refresh` to re-probe.",
            failures.join("\n")
        ))
    }
}

/// Work out which mirrors to use.
pub struct ResolveOptions<'a> {
    /// Mirrors the user named explicitly; when non-empty these are used as-is.
    pub explicit: &'a [String],
    /// Ignore the cache and re-probe.
    pub refresh: bool,
    /// Called with a human-readable note about what is happening.
    pub progress: &'a dyn Fn(&str),
}

/// Resolve a usable mirror pool, consulting cache and probing as needed.
pub fn resolve(policy: &NetPolicy, opts: ResolveOptions<'_>) -> Result<Pool> {
    // 1. An explicit choice is honoured without probing. If the user names a
    //    mirror, second-guessing them is just latency.
    if !opts.explicit.is_empty() {
        let mut ordered = Vec::new();
        for raw in opts.explicit {
            ordered.push(parse_mirror(raw, policy)?);
        }
        return Ok(Pool::new(ordered));
    }

    // 2. A recent probe result.
    if !opts.refresh
        && let Some(cached) = load_cache(policy)
        && !cached.is_empty()
    {
        return Ok(Pool::new(cached));
    }

    // 3. Probe the seeds.
    (opts.progress)("checking mirrors");
    let seeds: Vec<Uri> = SEED_MIRRORS
        .iter()
        .filter_map(|s| parse_mirror(s, policy).ok())
        .collect();
    let statuses = probe_all(&seeds, policy);
    let mut ordered = rank(&statuses);

    // 4. If nothing in the seed list works, ask any reachable mirror for a
    //    fresh list of siblings. This is what lets the tool survive the whole
    //    seed list going stale.
    if ordered.is_empty() {
        (opts.progress)("no seed mirror responded — discovering alternatives");
        let discovered = discover_from_any(&seeds, policy);
        if !discovered.is_empty() {
            let statuses = probe_all(&discovered, policy);
            ordered = rank(&statuses);
        }
    }

    if ordered.is_empty() {
        let detail = statuses
            .iter()
            .filter_map(|s| match &s.outcome {
                Outcome::Unhealthy(reason) => Some(format!(
                    "  {}: {reason}",
                    crate::net::host_of(&s.base).unwrap_or_default()
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "no working Libgen mirror found.\n{detail}\n\n\
             Libgen mirrors move often. If you know a current one, use \
             `--mirror <url>` or set it in {}.",
            config::config_path().display()
        );
    }

    save_cache(&ordered);
    Ok(Pool::new(ordered))
}

/// Try each seed's `mirrors.php` until one yields candidates.
fn discover_from_any(seeds: &[Uri], policy: &NetPolicy) -> Vec<Uri> {
    let http = Http::new(NetPolicy {
        request_timeout: PROBE_TIMEOUT,
        ..*policy
    });
    for seed in seeds {
        let found = discover(&http, seed, policy);
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the cached ranking, if it is present and fresh.
fn load_cache(policy: &NetPolicy) -> Option<Vec<Uri>> {
    let text = std::fs::read_to_string(config::mirror_cache_path()).ok()?;
    let mut lines = text.lines();
    let stamp: u64 = lines.next()?.trim().parse().ok()?;
    let age = now_secs().saturating_sub(stamp);
    if age > CACHE_TTL.as_secs() {
        return None;
    }
    let mirrors: Vec<Uri> = lines
        .filter_map(|line| {
            let url = line.split('\t').next()?.trim();
            if url.is_empty() {
                return None;
            }
            parse_mirror(url, policy).ok()
        })
        .collect();
    if mirrors.is_empty() {
        None
    } else {
        Some(mirrors)
    }
}

/// Persist the ranking. Cache failures are never fatal.
fn save_cache(ordered: &[Uri]) {
    let dir = config::cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut out = format!("{}\n", now_secs());
    for uri in ordered {
        out.push_str(&format!("{uri}\n"));
    }
    let _ = std::fs::write(config::mirror_cache_path(), out);
}

/// Remove the cached ranking.
pub fn clear_cache() -> Result<()> {
    let path = config::mirror_cache_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> NetPolicy {
        NetPolicy::default()
    }

    #[test]
    fn parses_mirrors_in_several_forms() {
        assert_eq!(
            parse_mirror("libgen.li", &policy()).unwrap().to_string(),
            "https://libgen.li/"
        );
        assert_eq!(
            parse_mirror("https://libgen.li/", &policy())
                .unwrap()
                .to_string(),
            "https://libgen.li/"
        );
        assert_eq!(
            parse_mirror("  https://libgen.li  ", &policy())
                .unwrap()
                .to_string(),
            "https://libgen.li/"
        );
    }

    #[test]
    fn mirror_parsing_enforces_the_network_policy() {
        // The guard applies to configured mirrors too, not just to redirects.
        assert!(parse_mirror("http://libgen.li", &policy()).is_err());
        assert!(parse_mirror("https://localhost", &policy()).is_err());
        assert!(parse_mirror("https://127.0.0.1", &policy()).is_err());
        assert!(parse_mirror("ftp://libgen.li", &policy()).is_err());
        assert!(parse_mirror("", &policy()).is_err());
    }

    #[test]
    fn private_mirrors_allowed_only_with_opt_in() {
        let relaxed = NetPolicy {
            allow_private_hosts: true,
            allow_http: true,
            ..policy()
        };
        assert!(parse_mirror("http://localhost:8080", &relaxed).is_ok());
    }

    #[test]
    fn every_seed_mirror_is_well_formed() {
        for seed in SEED_MIRRORS {
            assert!(parse_mirror(seed, &policy()).is_ok(), "bad seed: {seed}");
        }
    }

    #[test]
    fn ranking_puts_fastest_healthy_first_and_drops_failures() {
        let mk = |url: &str, outcome: Outcome| MirrorStatus {
            base: parse_mirror(url, &policy()).unwrap(),
            outcome,
        };
        let statuses = vec![
            mk(
                "a.libgen.example",
                Outcome::Healthy {
                    latency: Duration::from_millis(900),
                    results: 25,
                },
            ),
            mk("b.libgen.example", Outcome::Unhealthy("HTTP 500".into())),
            mk(
                "c.libgen.example",
                Outcome::Healthy {
                    latency: Duration::from_millis(120),
                    results: 25,
                },
            ),
        ];
        let ranked = rank(&statuses);
        assert_eq!(ranked.len(), 2, "unhealthy mirrors are dropped");
        assert_eq!(ranked[0].host().unwrap(), "c.libgen.example");
        assert_eq!(ranked[1].host().unwrap(), "a.libgen.example");
    }

    #[test]
    fn pool_falls_through_to_the_next_mirror() {
        let pool = Pool::new(vec![
            parse_mirror("dead.libgen.example", &policy()).unwrap(),
            parse_mirror("live.libgen.example", &policy()).unwrap(),
        ]);
        let mut retried = Vec::new();
        let (value, used) = pool
            .try_each(
                |base| {
                    if base.host().unwrap().starts_with("dead") {
                        Err(err!("boom"))
                    } else {
                        Ok(42)
                    }
                },
                |base, _| retried.push(base.host().unwrap().to_string()),
            )
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(used.host().unwrap(), "live.libgen.example");
        assert_eq!(retried, ["dead.libgen.example"]);
    }

    #[test]
    fn pool_reports_all_failures_when_nothing_works() {
        let pool = Pool::new(vec![
            parse_mirror("a.libgen.example", &policy()).unwrap(),
            parse_mirror("b.libgen.example", &policy()).unwrap(),
        ]);
        let result: Result<(i32, Uri)> = pool.try_each(|_| Err(err!("HTTP 500")), |_, _| {});
        let message = result.unwrap_err().to_string();
        assert!(message.contains("a.libgen.example"), "{message}");
        assert!(message.contains("b.libgen.example"), "{message}");
    }

    #[test]
    fn empty_pool_is_an_error() {
        let pool = Pool::new(Vec::new());
        let result: Result<(i32, Uri)> = pool.try_each(|_| Ok(1), |_, _| {});
        assert!(result.is_err());
    }

    #[test]
    fn discovery_only_accepts_plausible_libgen_hosts() {
        assert!(looks_like_libgen("libgen.li"));
        assert!(looks_like_libgen("libgen-mirror.example"));
        assert!(looks_like_libgen("library.genesis.example"));
        assert!(!looks_like_libgen("ads.doubleclick.net"));
        assert!(!looks_like_libgen("evil.example"));
    }

    /// The name has to be Libgen's, not merely mention it. A mirror page is
    /// written by a third party, and `libgen.` as a subdomain label is the
    /// cheapest way to have this hand back somebody else's host.
    #[test]
    fn a_libgen_shaped_subdomain_of_another_domain_is_refused() {
        assert!(!looks_like_libgen("libgen.attacker.example"));
        assert!(!looks_like_libgen("libgen.li.attacker.example"));
        assert!(!looks_like_libgen("genesis.evil.test"));
        // The real thing, including under a subdomain and a two-part suffix.
        assert!(looks_like_libgen("mirror.libgen.gs"));
        assert!(looks_like_libgen("libgen.co.uk"));
        assert!(looks_like_libgen("LIBGEN.LI"));
        // A trailing root dot must not shift the labels along.
        assert!(looks_like_libgen("libgen.li."));
        // Degenerate names have no registrable part to read.
        assert!(!looks_like_libgen("libgen"));
        assert!(!looks_like_libgen(""));
    }

    #[test]
    fn long_error_messages_are_shortened_for_display() {
        let long = "a".repeat(200);
        assert!(short_reason(&long).chars().count() <= 61);
        assert_eq!(short_reason("request failed: HTTP 500"), "HTTP 500");
    }
}
