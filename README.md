# tomesole

[![CI](https://github.com/ChrisThoma/tomesole/actions/workflows/ci.yml/badge.svg)](https://github.com/ChrisThoma/tomesole/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tomesole.svg)](https://crates.io/crates/tomesole)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A terminal client for searching and downloading from Library Genesis: a
full-screen interface and a scriptable CLI in one small Rust binary.

Libgen has no stable address and is run by people you have no reason to trust.
Most of the work here goes into those two problems: finding a mirror that
works, and treating everything it sends back as hostile until proven otherwise.

## Quick start

```sh
cargo install tomesole    # needs Rust 1.91 or newer, nothing else

tomesole                  # the full-screen interface
tomesole dune             # search from the command line, then pick
tomesole doctor           # check the whole setup
```

![tomesole demo: the landing wordmark, a live search for Dune with cover art, filtering by language, then the Library tab showing downloaded books with their jackets](https://raw.githubusercontent.com/ChrisThoma/tomesole/main/docs/demo.gif)

## The interface

`tomesole` with no arguments opens three tabs: **Search**, **Library** for the
books you already have, and **Settings**. `Tab` cycles through them, `1`/`2`/`3`
jumps straight to one, and `?` shows every key.

```
╭  1 SEARCH    2 LIBRARY    3 SETTINGS  ───────────────────── ● libgen.li ╮
│❯ dune                                                                   │
╰─────────────────────────────────────────────────────────────────────────╯
 e format all   l language English 6   a author all   x clear    sort: year ▼
   TITLE                AUTHOR         YEAR LANG    SIZE   FMT  STATUS ╭──╮
▌  Dune                 Frank Herbert  1965 English 781 KB epub library│▄▄│
   Dune Messiah         Frank Herbert  1969 English 922 KB epub        │██│
   Children of Dune     Frank Herbert  1976 English 1.1 MB pdf         │▀▀│
                                                                       ╰──╯
 tab library  ↑↓ move  ⏎ download  e format  l lang  a author  s sort  ? help
```

A search returns a lot of near-duplicates (*Das Kapital* comes back as forty
German editions with the English translations mixed in), so results narrow in
place without re-searching: `e`, `l` and `a` open pick-lists of the formats,
languages and authors actually present, `s` sorts by relevance, title, author,
year or size, and `x` clears every filter. Books you already have are marked
`library` with a green dot, `space` marks results for a batch download, and
`y` copies the highlighted book's MD5.

The highlighted book's cover is drawn beside its record: real pixels in kitty,
Ghostty, WezTerm and iTerm2, half-block characters anywhere else with colour.
On the Library tab the cover is read straight out of the downloaded file where
the format carries one. `--no-covers` turns covers off.

The **Library** tab lists every download, newest first, and persists between
sessions: `⏎` opens a book in your reader, `f` reveals it in the file manager,
`/` filters, and `d` forgets an entry without deleting the file. The
**Settings** tab edits the same values as the config file and saves as you go.

Downloads run one at a time, since parallel requests to a volunteer-run mirror
tend to get rate-limited.

## The CLI

The same functionality from the command line, for scripting and one-off
downloads:

```
$ tomesole --title "the rust programming language" -n 4

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

```sh
tomesole -a "Ursula K. Le Guin" -e epub    # by author, EPUB only
tomesole -t "moby dick" --first            # take the top hit, no prompt
tomesole --json -t dune --no-download      # machine-readable
tomesole get 1b9159991f7fb1b3910c0be9ebf7e595
tomesole history                           # what has been downloaded
tomesole open 1                            # read the newest of them
tomesole mirrors --refresh                 # which mirrors work right now
```

Any query, in the box or on the command line, can carry tags:
`author:herbert title:dune ext:epub`. `author:` `title:` `series:` `publisher:`
`year:` `isbn:` narrow which column is matched; `ext:` and `lang:` filter what
comes back; `topic:` picks the collection. Quote a value to keep spaces in it.
Anything else is left exactly as typed, so `Dune: Messiah` searches for
`Dune: Messiah` rather than being read as a tag. Selections accept numbers,
lists and ranges: `3`, `1,4,7`, `2-5`.

Every completed download is recorded in `tomesole/history.tsv` under
`$XDG_DATA_HOME`, mode `0600`, capped at the last thousand, so it can be found
again without going back to Libgen; `tomesole history --clear` forgets
everything, and `history = false` in the config stops recording. Which
application opens a book is `reader` in the config, or `--with`.
`tomesole config --init` writes a commented config file under
`$XDG_CONFIG_HOME`; flags always override it.

## Finding a live mirror

Mirror domains get seized, expire, or serve a perfectly healthy front page
while their search endpoint returns HTTP 500. So the health check runs a real
search and requires parseable results back: a mirror counts as up only if it
can do the thing it is needed for.

Mirrors you configured are used without second-guessing. Otherwise tomesole
reuses a ranking cached from a probe in the last 6 hours, or probes the
built-in list concurrently and ranks by search latency; if the entire list is
dead, it asks any reachable mirror for its own list of siblings, so a new
domain works without a new release. If the chosen mirror fails partway
through, the next one is tried automatically.

## Security

Every byte, header and filename from a mirror is treated as attacker-controlled.

- **Integrity:** files are verified against their catalogue MD5 as the bytes
  stream past, and moved into place only on a match; a mismatch is deleted.
  MD5 is broken for collision resistance, so this catches corruption and
  opportunistic substitution, not a crafted collision. It is the strongest
  check the catalogue offers.
- **Filenames** are built from catalogue metadata, never from the server's
  `Content-Disposition` (the obvious place to attempt a path traversal). Path
  separators, control characters and leading dots are stripped, Windows
  reserved names are escaped.
- **Extensions** are allowlisted: a "book" claiming to be `.exe`, `.dmg`,
  `.sh` or `.jar` is refused, and anything unrecognised is saved as `.bin` so
  it cannot be launched by double-clicking.
- **Network:** TLS is always verified, with deliberately no flag to disable
  it; cleartext HTTP needs `--allow-http`. Redirects are followed one hop at a
  time and re-validated at every hop against private and reserved address
  ranges, so a mirror cannot bounce the client onto localhost or a cloud
  metadata endpoint. URLs are parsed by the same `http::Uri` that opens the
  socket, not a second parser.
- **Responses** are size-capped twice, and an HTML response is rejected as a
  captcha or error page. Downloads land in a `0600` temp file and are renamed
  only after verification; existing files are never overwritten without
  `--force`. On macOS the result gets the same quarantine attribute a browser
  would set.
- **Covers** are the one place a mirror gets to name a URL that is then
  fetched, so the image must be on the same host, small, and begin with a real
  image signature. The in-tree JPEG decoder bounds-checks every length and
  index, and the tests fuzz it with truncated and corrupted files.
- **Opening a book** starts another program, so the path is canonicalised and
  passed as a single argument to `exec` with no shell in the chain.

## Dependencies and tests

Four dependencies on any one target: `ureq` for HTTPS, `ratatui` and
`crossterm` for the interface, plus `xattr` on Unix or `windows-sys` on
Windows. Everything else (argument parsing, HTML scanning, MD5, JSON output,
the JPEG decoder) is in-tree, each a few dozen lines against a dependency tree
that was considerably larger.

`cargo test` runs 300+ tests without touching the network: the MD5 vectors,
the SSRF and scheme guards, filename sanitising, HTML scanning against real
saved markup, the streaming download path against a throwaway loopback server,
JPEG decoding of truncated and corrupted files at every offset, and the
interface through ratatui's off-screen backend on terminals from 20×8 to
250×12.

## Legal

This is a client for a public search index. What you download with it, and
whether you are entitled to, is your call and your responsibility.
