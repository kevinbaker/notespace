#!/usr/bin/env node
// Downloads a window of Hacker News into NDJSON: one story per line, comments nested under
// `children`. Shape is HN's own -- every notespace-specific decision lives in the importer.
//
//   node scripts/hn-fetch.mjs --days 7 --out target/hn/hn.ndjson
//   node scripts/hn-fetch.mjs --days 7 --source bigquery --out target/hn/hn.ndjson
//
// Algolia needs no credentials and is current. BigQuery needs the `bq` CLI, a billed project,
// and its copy of the corpus stops in 2022 -- see --end.

import { createWriteStream } from "node:fs";
import { mkdir, writeFile, stat } from "node:fs/promises";
import { createInterface } from "node:readline";
import { createReadStream } from "node:fs";
import { execFile } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

const DEFAULTS = {
  days: 7,
  end: null,
  out: "target/hn/hn.ndjson",
  source: "algolia",
  minComments: 1,
  maxStories: 0,
  concurrency: 6,
  rate: 2.5,
  resume: false,
  bqProject: process.env.HN_BQ_PROJECT || "",
  bqTrailingDays: 3,
};

function parseArgs(argv) {
  const opts = { ...DEFAULTS };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith("--")) die(`unexpected argument ${arg}`);
    const eq = arg.indexOf("=");
    const key = (eq === -1 ? arg.slice(2) : arg.slice(2, eq)).replace(/-([a-z])/g, (_, c) =>
      c.toUpperCase(),
    );
    if (key === "resume") {
      opts.resume = true;
      continue;
    }
    if (key === "help") usage(0);
    const value = eq === -1 ? argv[++i] : arg.slice(eq + 1);
    if (value === undefined) die(`--${arg.slice(2)} needs a value`);
    if (!(key in opts)) die(`unknown option --${arg.slice(2)}`);
    opts[key] = typeof DEFAULTS[key] === "number" ? Number(value) : value;
  }
  if (!Number.isFinite(opts.days) || opts.days <= 0) die("--days must be a positive number");
  if (opts.source !== "algolia" && opts.source !== "bigquery") {
    die("--source must be 'algolia' or 'bigquery'");
  }
  return opts;
}

function usage(code) {
  process.stderr.write(
    `usage: node scripts/hn-fetch.mjs [options]

  --days N            window length in days (default 7)
  --end DATE|EPOCH    window end, exclusive (default: now, or the table max for bigquery)
  --out PATH          NDJSON destination (default target/hn/hn.ndjson)
  --source NAME       algolia | bigquery (default algolia)
  --min-comments N    skip stories with fewer comments (default 1)
  --max-stories N     stop after N stories (default 0 = no limit)
  --concurrency N     parallel item fetches, algolia only (default 6)
  --rate N            requests per second, algolia only (default 2.5)
  --resume            keep the existing output file and skip stories already in it
  --bq-project ID     billing project for bq (or set HN_BQ_PROJECT)
  --bq-trailing-days N  extra days of comments to pull so late replies land (default 3)
`,
  );
  process.exit(code);
}

function die(msg) {
  process.stderr.write(`hn-fetch: ${msg}\n`);
  process.exit(2);
}

function parseInstant(value) {
  if (value === null || value === undefined || value === "") return Math.floor(Date.now() / 1000);
  if (/^\d+$/.test(String(value))) return Number(value);
  const ms = Date.parse(String(value));
  if (Number.isNaN(ms)) die(`cannot parse --end ${value}`);
  return Math.floor(ms / 1000);
}

const iso = (secs) => new Date(secs * 1000).toISOString();

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Algolia allows roughly 10k requests an hour and answers 403 -- not 429 -- once you are over
// it, so the pace is enforced here rather than discovered by being cut off. Every rejection
// also widens the interval, because a fixed rate that is slightly too fast never recovers.
function rateLimiter(perSecond) {
  let interval = perSecond > 0 ? 1000 / perSecond : 0;
  let next = 0;
  return {
    async take() {
      if (interval === 0) return;
      const now = Date.now();
      const at = Math.max(now, next);
      next = at + interval;
      if (at > now) await sleep(at - now);
    },
    slower() {
      interval = Math.min(Math.max(interval, 50) * 1.5, 5000);
    },
    perSecond: () => (interval === 0 ? Infinity : 1000 / interval),
  };
}

// A shared 403 means the whole pool is over the limit, so retries wait behind the limiter too.
const RETRYABLE = new Set([403, 408, 429, 500, 502, 503, 504]);

async function getJson(url, limiter, { attempts = 6 } = {}) {
  let wait = 1000;
  for (let attempt = 1; ; attempt++) {
    if (limiter) await limiter.take();
    let res;
    try {
      res = await fetch(url, { headers: { "user-agent": "notespace-hn-import/0.1" } });
    } catch (err) {
      if (attempt === attempts) throw err;
      await sleep(wait);
      wait *= 2;
      continue;
    }
    if (res.ok) return res.json();
    if (attempt === attempts || !RETRYABLE.has(res.status)) {
      throw new Error(`GET ${url} -> ${res.status}`);
    }
    if (limiter) limiter.slower();
    await sleep(wait);
    wait *= 2;
  }
}

// ---------------------------------------------------------------------------
// Algolia
// ---------------------------------------------------------------------------

const ALGOLIA = "https://hn.algolia.com/api/v1";

// Algolia caps any one search at 1000 hits, so the window is walked backwards by timestamp
// rather than paged. Ties on created_at_i are why `seen` exists: the cursor has to be
// inclusive to avoid skipping a second story in the same second.
async function* algoliaStories(from, to, limiter, log) {
  let cursor = to;
  const seen = new Set();
  while (cursor > from) {
    const filters = `created_at_i>=${from},created_at_i<${cursor}`;
    const url =
      `${ALGOLIA}/search_by_date?tags=story&hitsPerPage=1000` +
      `&numericFilters=${encodeURIComponent(filters)}`;
    const page = await getJson(url, limiter);
    const hits = page.hits || [];
    if (hits.length === 0) return;

    let fresh = 0;
    let oldest = cursor;
    for (const hit of hits) {
      const id = Number(hit.objectID);
      oldest = Math.min(oldest, hit.created_at_i);
      if (seen.has(id)) continue;
      seen.add(id);
      fresh++;
      yield hit;
    }
    log(`  listed ${seen.size} stories, back to ${iso(oldest)}`);
    // Every hit in the last page was a duplicate: step past the tie or loop forever.
    cursor = fresh === 0 ? oldest : oldest + 1;
    if (cursor > to) return;
  }
}

function algoliaStoryKind(hit) {
  const tags = hit._tags || [];
  if (tags.includes("ask_hn")) return "ask";
  if (tags.includes("show_hn")) return "show";
  if (tags.includes("job")) return "job";
  if (tags.includes("poll")) return "poll";
  return "story";
}

async function fetchAlgolia(opts, out, skip, log) {
  const to = parseInstant(opts.end);
  const from = to - Math.round(opts.days * 86400);
  log(`window ${iso(from)} .. ${iso(to)} (${opts.days}d)`);

  const limiter = rateLimiter(opts.rate);
  const wanted = [];
  for await (const hit of algoliaStories(from, to, limiter, log)) {
    if ((hit.num_comments || 0) < opts.minComments) continue;
    if (skip.has(Number(hit.objectID))) continue;
    wanted.push(hit);
    if (opts.maxStories > 0 && wanted.length >= opts.maxStories) break;
  }
  log(`${wanted.length} stories to fetch (>= ${opts.minComments} comments)`);

  // One /items/ request returns a story's whole comment tree, so the pool is one task per story.
  let done = 0;
  let failed = 0;
  let comments = 0;
  const next = (() => {
    let i = 0;
    return () => (i < wanted.length ? wanted[i++] : null);
  })();

  const worker = async () => {
    for (let hit = next(); hit !== null; hit = next()) {
      let item;
      try {
        item = await getJson(`${ALGOLIA}/items/${hit.objectID}`, limiter);
      } catch (err) {
        failed++;
        log(`  ! ${hit.objectID}: ${err.message}`);
        continue;
      }
      item.num_comments = hit.num_comments ?? null;
      item.hn_kind = algoliaStoryKind(hit);
      if (item.points == null) item.points = hit.points ?? null;
      comments += countNodes(item) - 1;
      out.write(JSON.stringify(item) + "\n");
      done++;
      if (done % 50 === 0) {
        log(
          `  fetched ${done}/${wanted.length} stories, ${comments} comments` +
            ` (${limiter.perSecond().toFixed(1)}/s)`,
        );
      }
    }
  };

  await Promise.all(
    Array.from({ length: Math.max(1, Math.min(opts.concurrency, 16)) }, worker),
  );
  return { from, to, stories: done, comments, failed };
}

function countNodes(node) {
  let n = 1;
  for (const child of node.children || []) n += countNodes(child);
  return n;
}

// ---------------------------------------------------------------------------
// BigQuery
// ---------------------------------------------------------------------------

async function bq(args, log) {
  try {
    const { stdout } = await execFileAsync("bq", args, { maxBuffer: 1 << 30 });
    return stdout;
  } catch (err) {
    if (err.code === "ENOENT") {
      die(
        "bq not found. Install the Google Cloud SDK and authenticate:\n" +
          "    https://cloud.google.com/sdk/docs/install\n" +
          "    gcloud auth login && gcloud config set project YOUR_PROJECT\n" +
          "  Queries against bigquery-public-data bill YOUR project (1 TB/month is free).\n" +
          "  Or drop --source bigquery and use the credential-free Algolia path.",
      );
    }
    log(err.stderr || "");
    die(`bq failed: ${err.message}`);
  }
}

function bqBase(opts) {
  const args = ["--format=json", "--quiet", "--headless"];
  if (opts.bqProject) args.push(`--project_id=${opts.bqProject}`);
  return args;
}

const HN_TABLE = "bigquery-public-data.hacker_news.full";

async function fetchBigQuery(opts, out, skip, log) {
  let to = opts.end === null ? null : parseInstant(opts.end);
  if (to === null) {
    // The public copy stopped updating; defaulting to "now" would return an empty window.
    log("no --end given: asking BigQuery for the newest row in the table");
    const rows = JSON.parse(
      await bq(
        [...bqBase(opts), "query", "--nouse_legacy_sql", "--max_rows=1",
         `SELECT UNIX_SECONDS(MAX(timestamp)) AS t FROM \`${HN_TABLE}\``],
        log,
      ),
    );
    to = Number(rows[0].t) + 1;
    log(`table ends at ${iso(to)}`);
  }
  const from = to - Math.round(opts.days * 86400);
  const trailing = to + Math.round(opts.bqTrailingDays * 86400);
  log(`window ${iso(from)} .. ${iso(to)} (+${opts.bqTrailingDays}d of trailing replies)`);

  // The `full` table has no top_level_parent, so trees are rebuilt here from `parent`.
  const sql = `
SELECT id, \`by\` AS author, time, type, title, url, text, score, descendants, parent
FROM \`${HN_TABLE}\`
WHERE timestamp >= TIMESTAMP_SECONDS(${from})
  AND timestamp < TIMESTAMP_SECONDS(${trailing})
  AND (
    (type = 'story' AND timestamp < TIMESTAMP_SECONDS(${to}))
    OR type = 'comment'
  )
ORDER BY time`;
  log("running query (this scans a few GB and takes a minute)");
  const rows = JSON.parse(
    await bq([...bqBase(opts), "query", "--nouse_legacy_sql", "--max_rows=5000000", sql], log),
  );
  log(`${rows.length} rows returned`);

  const nodes = new Map();
  for (const row of rows) {
    nodes.set(Number(row.id), {
      id: Number(row.id),
      author: row.author || null,
      created_at_i: Number(row.time),
      type: row.type,
      title: row.title || null,
      url: row.url || null,
      text: row.text || null,
      points: row.score === null || row.score === undefined ? null : Number(row.score),
      num_comments:
        row.descendants === null || row.descendants === undefined ? null : Number(row.descendants),
      parent: row.parent === null || row.parent === undefined ? null : Number(row.parent),
      children: [],
    });
  }
  // Comments whose parent fell outside the window are dropped with their subtree.
  let orphans = 0;
  for (const node of nodes.values()) {
    if (node.parent === null) continue;
    const parent = nodes.get(node.parent);
    if (parent === undefined) orphans++;
    else parent.children.push(node);
  }
  for (const node of nodes.values()) node.children.sort((a, b) => a.created_at_i - b.created_at_i);
  if (orphans > 0) log(`${orphans} comments dropped: parent outside the window`);

  let stories = 0;
  let comments = 0;
  for (const node of nodes.values()) {
    if (node.type !== "story" || node.created_at_i >= to) continue;
    if (skip.has(node.id)) continue;
    const n = countNodes(node) - 1;
    if ((node.num_comments ?? n) < opts.minComments) continue;
    node.hn_kind = bqStoryKind(node);
    node.story_id = node.id;
    out.write(JSON.stringify(node) + "\n");
    stories++;
    comments += n;
    if (opts.maxStories > 0 && stories >= opts.maxStories) break;
  }
  return { from, to, stories, comments, failed: 0 };
}

function bqStoryKind(node) {
  const title = node.title || "";
  if (/^ask hn[:\s]/i.test(title)) return "ask";
  if (/^show hn[:\s]/i.test(title)) return "show";
  if (node.type === "job" || (node.author === null && node.url)) return "job";
  return "story";
}

// ---------------------------------------------------------------------------

async function existingIds(file) {
  const ids = new Set();
  try {
    await stat(file);
  } catch {
    return ids;
  }
  const rl = createInterface({ input: createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line.trim()) continue;
    try {
      ids.add(Number(JSON.parse(line).id));
    } catch {
      // A truncated final line from an interrupted run; the story is simply refetched.
    }
  }
  return ids;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const log = (msg) => process.stderr.write(`${msg}\n`);

  await mkdir(path.dirname(path.resolve(opts.out)), { recursive: true });
  const skip = opts.resume ? await existingIds(opts.out) : new Set();
  if (skip.size > 0) log(`resuming: ${skip.size} stories already in ${opts.out}`);

  const out = createWriteStream(opts.out, { flags: opts.resume ? "a" : "w" });
  const started = Date.now();
  const stats =
    opts.source === "bigquery"
      ? await fetchBigQuery(opts, out, skip, log)
      : await fetchAlgolia(opts, out, skip, log);
  await new Promise((resolve, reject) => out.end(resolve).on("error", reject));

  const meta = {
    source: opts.source,
    fetched_at: Math.floor(Date.now() / 1000),
    window_from: stats.from,
    window_to: stats.to,
    days: opts.days,
    min_comments: opts.minComments,
    stories: stats.stories + skip.size,
    comments: stats.comments,
    failed: stats.failed,
  };
  await writeFile(opts.out.replace(/\.ndjson$/, "") + ".meta.json", JSON.stringify(meta, null, 2));

  const secs = ((Date.now() - started) / 1000).toFixed(1);
  log(
    `\nwrote ${opts.out}: ${stats.stories} stories, ${stats.comments} comments in ${secs}s` +
      (stats.failed ? ` (${stats.failed} failed)` : ""),
  );
}

main().catch((err) => {
  process.stderr.write(`hn-fetch: ${err.stack || err.message}\n`);
  process.exit(1);
});
