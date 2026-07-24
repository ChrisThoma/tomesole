# clibgen

A terminal client for searching and downloading from Library Genesis, with both
a full-screen interface and a scriptable CLI.

Libgen has no stable address and is run by people you have no reason to trust.
Most of the work here goes into those two problems: finding a mirror that
actually works, and treating everything it sends back as hostile until proven
otherwise.

Run `clibgen` with no arguments for the full-screen interface. It has two tabs —
**Search** for finding books, **Library** for the ones you already have —
and `Tab` (or `1`/`2`) moves between them from anywhere, including mid-search:

```
╭  1 SEARCH    2 LIBRARY 12  ──────────────────────────────── ● libgen.li ╮
│❯ dune                                                                   │
╰─────────────────────────────────────────────────────────────────────────╯
 e format all   l language English 6   a author all   x clear   s sort year ▼
   TITLE                AUTHOR         YEAR LANG     SIZE   FMT  STATUS ╭──╮
▌  Dune                 Frank Herbert  1965 English 781 KB epub library│▄▄│
   Dune Messiah         Frank Herbert  1969 English 922 KB epub        │██│
   Children of Dune     Frank Herbert  1976 English 1.1 MB pdf         │▀▀│
                                                                       ╰──╯
 tab library  ↑↓ move  ⏎ download  e format  l lang  a author  s sort  ? help
```

The interface paints its own canvas — a deep blue-grey with zebra-striped
rows — rather than inheriting the terminal's colours, and colour carries
meaning: teal is interactive, amber is emphasis, every file format keeps one
hue of its own (`epub` green, `pdf` coral, `djvu` violet…) drawn as a solid
chip, and languages share one calm blue. On a wide terminal the right-hand
panel shows the cover near poster size with the record beneath it as labelled
rows; narrower ones get a compact strip along the bottom.

A search brings back whatever the mirror has — *Das Kapital* is forty German
editions with the English translations scattered among them. So the results
can be carved in place, no re-search needed. `e`, `l` and `a` each open a
little menu of the values actually present — the formats, the languages, the
authors — every one with a live count of what choosing it would leave. It is a
pick-list, not a cycle: type a fragment to narrow the menu (`guin` finds *Le
Guin, Ursula* among eighty names), arrow or Enter to choose, and `⇥` rotates
between the three facets without leaving the pop-over. The bar under the search
box always says how much of the catch is on screen (`4 of 40 shown`). The
filters are standing preferences — they survive the next search — and `x`
clears them all.

A ragged pile also wants an order. `s` opens a little menu of the orders —
relevance (the mirror's own guess), title, author, year, size — with the one in
force marked and its direction shown; pick a column and `←`/`→` flip the way it
runs, or `S` reverses in place without opening anything. So "the smallest EPUB"
or "the newest edition" is a choice rather than a squint down the `SIZE` column.
Size and year start biggest- and newest-first, because that is nearly always
what you meant. An edition you have downloaded
before is marked `library` in its own column and with a green dot, so you
never fetch the same book twice by accident; and when several results are
marked for a batch, the strip totals what they will pull (`3 marked · 24 MB`)
before you commit. `y` lifts the highlighted book's MD5 onto the clipboard.

The **Library** tab lists every book you have downloaded, newest first, and
persists between sessions — it is a view onto `~/.local/share/clibgen/history.tsv`.
A strip under the box sums up the shelf at a glance — `12 books · 1.2 GB · 8
epub · 3 pdf · 1 mobi` — and the same `s` sort orders it by title, author, size
or when it was added. `⏎` opens the highlighted book in your reader, `f`
reveals it in the file manager, `/` filters the list by title, author or
filename, and `d` forgets an entry without touching the file. A book whose file
has since moved is shown greyed out and marked `missing`.

Searching and downloading happen on worker threads, so the interface stays
responsive while a large file comes down. Downloads run one at a time — firing
parallel requests at a volunteer-run mirror is a good way to get rate-limited.

The cover is drawn as real pixels in kitty, Ghostty, WezTerm and iTerm2, and as
half-block characters — sketched above — anywhere else. On the Library tab the
cover comes out of the book file you already downloaded — an EPUB or CBZ is a
zip with the image inside, a MOBI/AZW embeds it — so it needs no network at all;
only when the file carries none does it fall back to a cover cached from an
earlier search.

| key | does |
| --- | --- |
| `Tab` `1` `2` | switch between the Search and Library tabs |
| `/` `i` | type in the box (search terms, or a library filter) |
| `⏎` | search / download the selection, or open a library book |
| `↑` `↓` `k` `j` | move; `PgUp`/`PgDn` for ten, `g`/`G` for ends |
| `e` `l` `a` | open the format / language / author menu (type to narrow, `⏎` to choose, `⇥` to switch facet); `x` clears every filter |
| `s` | open the sort menu (relevance, title, author, year, size; `←`/`→` set direction); `S` reverses in place |
| `space` | mark a result; `A` marks or unmarks everything showing |
| `y` | copy the highlighted book's MD5 to the clipboard |
| `o` `f` | open a downloaded book, or show it in the file manager |
| `d` | forget a library entry (the file stays) |
| `r` | re-run the search / re-read the library |
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

## Searching by field

Any query — in the box or on the command line — can carry tags:

```
author:herbert title:dune ext:epub
title:"the dispossessed" lang:english
le guin year:1974
```

`author:` `title:` `series:` `publisher:` `year:` `isbn:` narrow which column
is matched. `ext:` and `lang:` filter what comes back. `topic:` picks the
collection. Quote a value to keep spaces in it.

Anything that is not one of those keys is left exactly as typed, so `Dune:
Messiah` searches for `Dune: Messiah` rather than being read as a tag.

## Covers

The full-screen interface shows the cover of whatever is highlighted, once the
selection has been still for a moment — a cover costs two requests, and firing
those off while somebody holds a cursor key down would be rude to a
volunteer-run mirror. Answers are cached, including "this one has no cover".

How it draws depends on what the terminal can do:

| terminal | how |
| --- | --- |
| kitty, Ghostty, WezTerm | kitty graphics protocol — real pixels |
| iTerm2 | inline image protocol — real pixels |
| anything else with colour | half-block characters, two pixels per cell |

A search result is only an MD5, so its cover is fetched from the mirror as
above. A book in your library is a file on disk, and most formats carry their
own jacket — so the Library tab reads it straight out of the file (EXTH record
201 in a MOBI, the cover image inside an EPUB or CBZ zip) with no request at
all. Every byte is treated as hostile: offsets and lengths are bounds-checked,
nothing is decompressed in place, the extracted bytes must still begin with a
real image signature, and a truncated or malformed book yields no cover rather
than a crash.

Detection is from the environment, never by writing a query escape and waiting
for a reply — that races with the keyboard reader for the answer. `clibgen
doctor` reports which one it picked. `--no-covers`, or `covers = false` in the
config, turns the whole thing off.

Since two of those three paths need pixels rather than a file, there is a
baseline JPEG decoder in-tree (`src/jpeg.rs`). It is checked against Apple's
decoder on a real cover: mean absolute error 0.54 per channel, maximum 4.

## Past downloads

Every completed download is recorded — what it was, where it went, and whether
it verified — so it can be found again later without going back to Libgen.

```
$ clibgen history

    #  Title                            Author           When         Size  Fmt
  ───  ───────────────────────────────  ───────────────  ───────  ────────  ─────
    1  Navigators of Dune               Brian Herbert    2h ago    2.10 MB  epub
    2  The Dispossessed                 Ursula K. Le Gu… 3d ago     781 KB  epub
    3  The Rust Programming Language     Steve Klabnik   2026-06…  3.00 MB  pdf

$ clibgen open 1                # in your reader
$ clibgen reveal 1              # in Finder / your file manager
$ clibgen open --with Preview 2
$ clibgen open --find dispossessed
```

`clibgen open` with no argument takes the most recent. Inside the interface,
`h` shows the same list: `⏎` opens, `f` reveals, `d` forgets an entry without
touching the file.

The list is a table under `~/.local/share/clibgen/history.tsv`, mode `0600`,
capped at the last thousand downloads. A file that has since been moved or
deleted stays listed and is marked as missing, rather than quietly vanishing.
`clibgen history --clear` forgets everything, cached covers included; `history =
false` in the config stops it recording at all.

Which application opens a book is `reader` in the config, or `--with`. On macOS
that names an application (`Books`, `Preview`); elsewhere it is a command run
with the file as its argument (`zathura`). Unset means the system default.

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
clibgen author:herbert title:dune         # the same, as tags
clibgen -t "moby dick" --first            # take the top hit, no prompt
clibgen --json -t dune --no-download      # machine-readable
clibgen get 1b9159991f7fb1b3910c0be9ebf7e595
clibgen history                           # what has been downloaded
clibgen open 1                            # read the newest of them
clibgen mirrors --refresh                 # which mirrors work right now
clibgen doctor                            # check the whole setup
```

`history`, `open` and `reveal` are also ordinary words to want a book about, so
they only mean the command when what follows fits: `clibgen history` lists
downloads, `clibgen history of the peloponnesian war` searches for one.

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

**Covers** are the one place a mirror gets to choose a URL we then fetch, so:
the image URL has to be on the same host as the mirror that named it, the
response has to be small, and it has to actually begin with an image signature
— a captcha page served in its place is discarded rather than cached. Cache
filenames come from the record's MD5, never from the URL. The JPEG decoder
checks every length, table id and index the file gives it before use, and is
fuzzed in the tests by truncating and corrupting a real image at every offset.

**Opening a file** starts another program, so the path is passed as a single
argument to `exec` with no shell anywhere in the chain, and it is canonicalised
first — which both proves the file is still there and makes it absolute, so a
filename cannot be read as a flag by whatever it is handed to.

## Dependencies

Four: `ureq` for HTTPS, `ratatui` and `crossterm` for the full-screen interface,
and `xattr` on Unix for the macOS quarantine flag.

Everything else is in-tree — argument parsing, HTML scanning, MD5, JSON output,
base64, the JPEG decoder, the CLI progress bar, table layout and terminal
sizing — because each was a few dozen lines against a dependency tree that was
considerably larger. MD5 is
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

268 tests, no network needed. They cover the MD5 vectors, the SSRF and scheme
guards, filename sanitising against traversal and executable extensions, HTML
scanning against real markup, and the streaming download path — including that
a file failing its checksum is discarded and leaves nothing behind — against a
throwaway loopback server.

The JPEG decoder is tested against real files produced by `sips`, colour and
greyscale, and then hardened by decoding a truncated copy at every length and a
corrupted copy at every third byte, requiring an error or a picture but never a
panic. The history file is round-tripped with tabs and newlines embedded in the
metadata, since the format is tab-separated and the metadata comes from Libgen.

One test is `#[ignore]`d because it needs the network: `cargo test -- --ignored
covers_can_be_fetched` fetches real covers from a live mirror, which is how the
hotlink protection on cover images was found in the first place. A second,
`dump_design_previews`, renders the interface's main states to a styled HTML
page for design review — set `CLIBGEN_PREVIEW_DIR` to somewhere writable and
run it with `--ignored`.

The TUI is tested through ratatui's off-screen backend: key handling, cover
placement, and that it draws without panicking from an 20×8 terminal up to
250×60. Small terminals
matter — an earlier version silently clipped the download progress bar out of
the details pane because the box was one row too short.

## Legal

This is a client for a public search index. What you download with it, and
whether you are entitled to, is your call and your responsibility.
