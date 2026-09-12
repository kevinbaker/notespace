# Importing Hacker News

Real threads, real nesting, real prose — a corpus to look at the read path with, and to measure
it against, that nobody had to write by hand.

```sh
./scripts/hn-import.sh                    # 7 days into local D1
./scripts/hn-import.sh --days 2           # a smaller bite
./scripts/hn-import.sh --skip-fetch       # re-import the NDJSON already on disk
npx wrangler dev --local                  # then browse /
```

Two stages, deliberately separate. `scripts/hn-fetch.mjs` downloads HN into NDJSON that mirrors
HN's own shape; `crates/hn-import` turns that into notespace SQL. Every notespace-specific
decision lives in the second stage, so re-deciding one costs a re-import and not a re-download.

## Where the data comes from

**Algolia (default).** `hn.algolia.com/api/v1` — no credentials, current to the minute, and one
`/items/{id}` request returns a story's entire comment tree. Rate-limited to about 10,000
requests an hour, and it answers `403` rather than `429` when you exceed it, so the fetcher
paces itself at 2.5 requests/second and widens that interval on every rejection. A 7-day window
is roughly 6,400 stories, of which ~2,400 have at least one comment; fetching those takes about
13 minutes and yields ~2,300 threads and ~52,000 posts.

**BigQuery (`--source bigquery`).** `bigquery-public-data.hacker_news.full`, which is what you
asked about and is the worse option for this:

- it needs the `bq` CLI, a Google account, and a **billing-enabled project** — queries against
  public datasets bill the *querying* project, not the publisher (1 TB/month is free, and this
  query is a few GB)
- the public copy **stopped being updated in 2022**, so a window ending "now" returns nothing.
  With no `--end` the fetcher asks the table for its own newest row and works back from there.
- the `full` table has no `top_level_parent` column, so comment trees have to be rebuilt from
  `parent` locally. Replies whose parent falls outside the window are dropped with their
  subtree; `--bq-trailing-days` (default 3) pulls extra comments so late replies still land.

It exists because the table is the canonical dump and someone will want a 2015 window. For
looking at what the site feels like with real content, use the default.

## What maps to what

| Hacker News | notespace |
|---|---|
| story | a `thread`, `kind` from its tags: `question`, `link`, `discussion`, `announcement` |
| story's own text | the thread's opening post, at path `0001` |
| top-level comment | a **sibling** of the opening post, not a reply to it |
| reply | a `post` nested by materialized path |
| `[dead]` / `[deleted]` | a post with `state = 'deleted'` — a tombstone, so the tree keeps its shape |
| a comment on a merged thread | imported once; the second copy is dropped |
| author | a `user`, name folded to notespace's rules |
| points | `thread.score` and `thread.rank` |
| ask_hn / show_hn / job / everything else | `hn/ask`, `hn/show`, `hn/jobs`, `hn/links` |

Comment bodies are HN's own HTML — `<p>`, `<i>`, `<a>`, `<pre><code>`, and entity-escaped text.
They are converted to markdown and then rendered by `notespace_render::markdown_to_html`, so
`body_html` is byte-identical to what the live write path would have stored. Measuring a read
path against hand-written HTML measures nothing.

Prose is escaped conservatively on the way in: `*`, `` ` ``, `[`, `]` and `\` become literals,
because HN does not render them as markup and a comment should not sprout emphasis it never had.
`_` is left alone (CommonMark ignores intraword underscores, and escaping it mangles every
`snake_case` identifier), and so is a leading `>` — HN's quoting convention becoming a real
blockquote is an improvement, not a distortion.

## Re-running it

Imported threads and posts get ids from 1,000,000 up, and the spaces all live under `hn/`. That
is what makes `--reset` — on by default — a scoped range delete: a previous import is replaced,
and anything else in the database, including the `scripts/spike.sh` seed and any real account,
is untouched. `--no-reset` appends instead.

Authors are inserted by name with `ON CONFLICT(name) DO NOTHING` and referenced through a lookup
on the unique name index, so an account that already exists is reused rather than duplicated.

Items are deduplicated by HN id across the whole import. HN merges threads, and a merged comment
comes back under both stories — in a 7-day window that happened once, and it is a failed insert
on `post.public_id` rather than a duplicate you would notice by reading.

Tombstones are a BigQuery-only concern in practice: Algolia's item endpoint omits dead and
deleted comments rather than returning them empty, so a 7-day Algolia import contains none.
The mapping is there because the `full` table does return them.

## Against the deployed database

`--remote` works and prompts before it does anything. Read the numbers first: D1's free plan
allows **100,000 rows written per day** and **500 MB** of storage, and a 7-day import is about
69,000 rows — 52,000 posts, 2,300 threads, 15,000 users. One import fits in a day's budget; two
do not. Two days of HN is a better size for a deployed demo.

## Options

`scripts/hn-import.sh` passes these through to the two stages:

```
--days N            window length (default 7)
--end DATE          window end, exclusive (default now; for bigquery, the table's newest row)
--source NAME       algolia | bigquery
--min-comments N    skip stories with fewer comments (default 1)
--max-stories N     stop after N stories
--rate N            requests per second (default 2.5)
--resume            keep the NDJSON already fetched and continue
--skip-fetch        re-import what is on disk
--space KEY         top-level space key (default hn)
--max-depth N       hard nesting limit, 1..32 (default 32)
--no-reset          append instead of replacing a previous import
--local | --remote  which D1 (default local)
```

A reply deeper than `--max-depth` attaches to the deepest ancestor that fits, rather than being
dropped — dropping it would take its whole subtree with it. At the default of 32 this never
fires on real HN data; it is there so `--max-depth 4` produces a usable shallow corpus for
comparing render cost against nesting.
