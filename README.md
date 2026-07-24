# clibgen

A terminal client for searching and downloading from Library Genesis, with both
a full-screen interface and a scriptable CLI.

Libgen has no stable address and is run by people you have no reason to trust.
Most of the work here goes into those two problems: finding a mirror that
actually works, and treating everything it sends back as hostile until proven
otherwise.

Run `clibgen` with no arguments for the full-screen interface:

```
╭ clibgen ──────────────────────────────────────────────── libgen.li ╮
│dune                                                                │
╰────────────────────────────────────────────────────────────────────╯
  Title                          Author           Year Lang    Size   Fmt
  Dune 1                         Frank Herbert         French  781 KB epub
  Sisterhood of Dune             Brian Herbert    2012 English 1000 KB epub
  Heretics of Dune               Frank Herbert    1984 English  937 KB azw3
╭────────────────────────────────────────────────────────────────────╮
│Dune 1                                                              │
│Frank Herbert                                                       │
│Presses Pocket  ·  French  ·  97 pages  ·  781 KB                   │
│md5 e7c75dc2964ce80c19cb69140aae8614                                │
╰────────────────────────────────────────────────────────────────────╯
 ↑↓ move  space mark  ⏎ download  / search  m mirrors  ? help  q quit
```

Searching and downloading happen on worker threads, so the interface stays
responsive while a large file comes down. Downloads run one at a time — firing
parallel requests at a volunteer-run mirror is a good way to get rate-limited.

| key | does |
| --- | --- |
| `/` `i` | edit the search query |
| `⏎` | run the search, or download the selection |
| `↑` `↓` `k` `j` | move; `PgUp`/`PgDn` for ten, `g`/`G` for ends |
| `space` | mark a result; `a` marks or unmarks everything |
| `r` | run the search again |
| `m` | re-probe mirrors and pick a new one |
| `?` | key help |
| `q` `esc` `^c` | quit |

The same thing from the CLI, for scripting and one-off grabs:

```
$ clibgen --title "the rust programming language" -n 4

  4 results from libgen.li

    #  Title                          Author               Year  Lang          Size  Fmt
  ───  ─────────────────────────────  ───────────────────  ────  ────────  ────────  ─────
    1  The Rust Programming Languag…  Steve Klabnik        2019  English    3.00 MB  epub
    2  The Rust Programming Languag…  Steve Klabnik        2019  English    5.00 MB  pdf
    3  The Rust Programming Languag…  Steve Klabnik        2019  English    3.00 MB  lit
    4  The Rust Programming Languag…  Steve Klabnik        2019  English    5.00 MB  fb2

  Select 1-4 (e.g. 3, or 1-4; Enter to cancel) › 1

  ↓ The Rust Programming Language
  ✓ Steve Klabnik - The Rust Programming Language (2019).epub
    ~/Downloads · 3.00 MB · MD5 verified
```

## Install

```sh
cargo build --release
cp target/release/clibgen ~/.local/bin/
```

Needs Rust 1.85 or newer (2024 edition). No other build dependencies.

## Use

```sh
clibgen                                   # full-screen interface
clibgen tui dune                          # interface, query pre-filled
clibgen dune                              # search, then pick
clibgen -a "Ursula K. Le Guin" -e epub    # by author, EPUB only
clibgen --title dune --lang english -n 50
clibgen -t "moby dick" --first            # take the top hit, no prompt
clibgen --json -t dune --no-download      # machine-readable
clibgen get 1b9159991f7fb1b3910c0be9ebf7e595
clibgen mirrors --refresh                 # which mirrors work right now
clibgen doctor                            # check the whole setup
```

Selections accept single numbers, lists, and ranges: `3`, `1,4,7`, `2-5`.

`clibgen config --init` writes a commented config file to
`~/.config/clibgen/config.conf`. Flags always override it.

## Finding a live mirror

Mirror domains get seized, expire, or start serving an anti-bot challenge. The
failure that drives the design is subtler: a mirror will serve a perfectly
healthy front page while its search endpoint returns HTTP 500. During
development, three of the nine built-in mirrors were doing exactly that.

So the health check runs a **real search and requires parseable results back**.
A mirror counts as up only if it can do the thing it is needed for.

In order, `clibgen`:

1. uses mirrors you configured explicitly, without second-guessing them;
2. falls back to a cached ranking from a probe in the last 6 hours;
3. otherwise probes the built-in list concurrently and ranks by search latency;
4. and if the entire built-in list is dead, asks any reachable mirror for its
   own list of siblings, so a new domain works without a new release.

The result is a pool, not a single pick. If the chosen mirror fails partway
through, the next one is tried automatically.

```
$ clibgen mirrors

  Mirror     Status    Detail
  ─────────  ────────  ────────────────────────────
  libgen.la  up        1248 ms, 25 results
  libgen.li  up        1594 ms, 25 results
  libgen.bz  up        2105 ms, 25 results
  libgen.gs  down      nodename nor servname provided, or not known
```

## What it refuses to do

Every byte, header and filename from a mirror is treated as attacker-controlled.

**Integrity.** Libgen indexes files by MD5, which gives a free end-to-end check.
The hash is computed as the bytes stream past, and the file is moved into place
only if it matches. A mismatch is deleted, not quarantined for later. To be
clear about the limits: MD5 is broken for collision resistance, so this catches
corruption, truncation and opportunistic substitution — not an attacker who can
craft a collision. It is the strongest check the catalogue offers.

**Filenames** are built from catalogue metadata, never from the server's
`Content-Disposition` — that header is the mirror's to choose and the obvious
place to attempt a path traversal. Path separators, control characters and
leading dots are stripped, Windows reserved names are escaped, and long titles
are truncated on character boundaries.

**Extensions** are allowlisted. A "book" claiming to be `.exe`, `.dmg`, `.sh` or
`.jar` is refused outright; anything unrecognised is saved as `.bin` so it
cannot be launched by double-clicking. A title ending in `.exe` becomes
`-exe` rather than producing a `payload.exe.epub` double extension.

**Network.** TLS is always verified and there is deliberately no flag to disable
it. Cleartext `http` is refused without `--allow-http`. Redirects are followed
manually, one hop at a time, and re-validated at every hop, so a mirror cannot
bounce the client onto `localhost`, a link-local address, or a cloud metadata
endpoint. Hostnames are resolved and checked against private and reserved ranges
before connecting.

URLs are validated with ureq's re-exported `http::Uri` rather than a separate
URL crate, so the address being checked is parsed by exactly the same code that
opens the socket — a second parser would reintroduce the parsing differential
the guard exists to prevent.

**Other.** Responses are size-capped twice, against the advertised
`Content-Length` and again against bytes actually received. An HTML response is
rejected as a captcha or error page. Downloads land in a `0600` temp file and
are renamed only after verification. Existing files are never overwritten
without `--force`. On macOS the result gets the same quarantine attribute a
browser would set, so Gatekeeper still gets a say.

## Dependencies

Four: `ureq` for HTTPS, `ratatui` and `crossterm` for the full-screen interface,
and `xattr` on Unix for the macOS quarantine flag.

Everything else is in-tree — argument parsing, HTML scanning, MD5, JSON output,
the CLI progress bar, table layout and terminal sizing — because each was a few
dozen lines against a dependency tree that was considerably larger. MD5 is
implemented directly rather than pulled in because the algorithm is fixed and
verifiable against the published RFC 1321 vectors, which the tests check.

The TUI is the one place that judgement went the other way. Layout, diffed
redrawing, resize handling and input decoding are real work, and getting them
subtly wrong is very visible, so ratatui and crossterm earn their place. They
account for roughly two thirds of the crate count; the CLI alone builds from
about thirty.

The HTML scanner is worth a caveat: it is a scanner, not a parser, written to
tolerate the specific sloppiness Libgen emits — unclosed rows, markup inside
attribute values, and stray doubled quotes — and to return nothing rather than
misbehave on anything else. It is tested against a saved copy of a real results
page.

## Tests

```sh
cargo test
```

150 tests, no network needed. They cover the MD5 vectors, the SSRF and scheme
guards, filename sanitising against traversal and executable extensions, HTML
scanning against real markup, and the streaming download path — including that
a file failing its checksum is discarded and leaves nothing behind — against a
throwaway loopback server.

The TUI is tested through ratatui's off-screen backend: key handling, and that
it draws without panicking from an 20×8 terminal up to 250×60. Small terminals
matter — an earlier version silently clipped the download progress bar out of
the details pane because the box was one row too short.

## Legal

This is a client for a public search index. What you download with it, and
whether you are entitled to, is your call and your responsibility.
