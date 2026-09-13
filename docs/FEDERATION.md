# Federation, and what it would mean for notespace

A primer, then a design sketch. Written for someone who has not used the fediverse; the second
half assumes the rest of this repository.

---

## 1. What "federation" means

Email is federated. You have an address at one provider, I have one at another, and we can
write to each other because both providers speak SMTP and agree on what an address looks like.
Neither of us needed an account on the other's server, and neither provider needed the other's
permission. Contrast Twitter or Reddit: one company runs one server, and if you are not on it
you cannot take part.

The **fediverse** is that idea applied to social software: many independently run servers
("instances") that speak a common protocol, so an account on one can follow, reply to and be
read from any other. The protocol nearly everyone uses is **ActivityPub**, a W3C standard from
2018. The best-known software is **Mastodon** (Twitter-shaped), but the same protocol carries
**Lemmy**, **PieFed** and **Mbin** (Reddit-shaped: communities of threads with nested
comments), **PeerTube** (video), **Pixelfed** (photos), **WriteFreely** (blogs), and since 2024
the forum software **NodeBB** and a **Discourse** plugin. Meta's Threads federates outbound.
An account on any of these can interact with content on any other, within limits each
piece of software chooses.

Two consequences matter for a forum:

- **A forum's threads can be followed and replied to from elsewhere.** Someone on Mastodon can
  follow `@general@notespace.org` the way they follow a person, see new threads in their
  timeline, and reply from there; the reply appears in the thread here. Someone on Lemmy sees
  the space as a community and can subscribe to it.
- **Nobody has to register here to take part**, which is the point, and also the problem: every
  moderation question the forum has answered for its own members it now has to answer for
  strangers whose accounts it does not control.

There is a second, newer protocol worth knowing the name of: **AT Protocol** (Bluesky). Its
model is different -- your data lives in a personal repository you can move between hosts,
and "app views" index everything globally -- and it is shaped for a firehose of short posts
rather than for communities with their own rules. For a forum it is not the natural fit, and
the two ecosystems do not talk to each other except through bridges. The rest of this document
is about ActivityPub.

## 2. How ActivityPub works, in one page

Everything is a JSON document at a URL.

**Actors.** A user is a `Person`; a community, board or space is a `Group`. Each has an
`inbox` (a URL others POST to) and an `outbox` (a URL others read), and a public key. Actors
are found by **WebFinger**: a request to
`https://notespace.org/.well-known/webfinger?resource=acct:general@notespace.org` answers with
the actor's URL. That is what makes `@general@notespace.org` an address.

**Objects.** A post is a `Note` (short, Mastodon-style) or `Page`/`Article` (a titled thing;
Lemmy uses `Page` for a thread and `Note` for a comment). Objects have an `id` (their URL), an
`attributedTo` (who wrote it), and, for a reply, `inReplyTo` (what it answers). notespace's
`/p/{id}` and `/t/{id}` are already stable URLs, which is most of what an object needs.

**Activities.** Things that happen are wrapped: `Create` (a new object), `Update`, `Delete`,
`Like`, `Announce` (a boost -- also how a Group tells its followers about something),
`Follow`/`Accept`, `Undo`, `Flag` (a report), `Block`. An activity is POSTed to the inbox of
whoever should know. When alice on Mastodon replies to a thread here, her server POSTs a
`Create{Note, inReplyTo: <our post>}` to our inbox; we store it, and, if the space is a Group,
we `Announce` it to everyone following the space so their servers get it too. That last step
is the Lemmy/FEP-1b12 pattern, and it is what makes a Group different from a Person: the group
re-broadcasts what it receives.

**Authentication.** Inbox POSTs carry an **HTTP Signature** made with the sending actor's
private key; the receiver fetches the actor document, gets the public key, and verifies. There
are no accounts, only keys. Outbound requests are signed the same way, and many servers now
demand signed *GET*s too ("authorized fetch"), so even reading a remote object means signing.

**Delivery.** Every event fans out: a new thread in a space with followers on 300 instances is
300 signed POSTs, retried with backoff when a server is down, forever-ish. Every server keeps a
delivery queue. Inbound, the same in reverse: a popular community's server receives a steady
stream of POSTs whether or not anyone here is reading.

**What you store.** Remote actors (name, keys, avatar, inbox URL), remote objects that arrived
(the replies themselves), and follow relationships in both directions. A federating server's
database fills with other people's content; that is the design, not a leak.

**What you moderate.** All of it. Remote replies land in your threads under your rules; you
can delete locally (the origin keeps its copy), `Flag` back to the origin server, and block
actors, or whole instances (**defederation**), which is the tool everyone reaches for first.

## 3. What it would take for notespace

The good news is that the read path is unaffected: baked pages, the version-keyed cache and
the `Store` seam do not care where a post came from. Everything else touches something.

### The parts that already fit

- **Stable public URLs for every thread and post.** ActivityPub needs an object to have one
  URL forever; §4.2 and §4.7 gave us that for reasons of our own.
- **Spaces are Groups.** `general/` becomes `@general@notespace.org`, and the hierarchy is a
  detail -- `hn/links/` is `@hn.links@` or `@hn-links@`; the key rules already forbid the
  characters that would make that ambiguous.
- **Posts render at write time.** A remote reply is HTML from a server we do not trust. It goes
  through `ammonia` like a local one and is stored as `body_html`; the source is whatever the
  origin sent, kept in `body_md` for the record.
- **Tier 0 runs on every write.** A remote reply is a write. The heuristics, the classifier,
  the queue, the review page and the log apply unchanged; the "new account" signal becomes
  "actor first seen", which is the same idea.
- **Cloudflare Queues** is the delivery queue we would otherwise have to build.
- **The action log** already has an `actor_kind`; a remote actor is one more kind, and `Flag`
  activities are reports.

### The parts that do not

- **Signing and verifying.** HTTP Signatures are RSA-SHA256 (RSA 2048, the ecosystem's de
  facto requirement; Ed25519 is arriving but not universal). RSA in wasm is slow and large;
  the Worker should use `crypto.subtle` instead, which is fast and already there. Every
  inbound POST costs a verification and, the first time, a fetch of the actor document.
  Every outbound POST costs a signature. All within budget; none free.
- **Remote identity.** A `remote_actor` table (URL, keys, inbox, display name, when last
  fetched), and `post.author_id` has to be able to point at one. Usernames like
  `alice@mastodon.social` need rendering, and `/u/{name}` a second form.
- **Storage.** Every remote reply is a row in D1, plus the actor. A community that gets
  popular on Lemmy fills the 500 MB working set with other servers' text. The R2 archive in
  §4.8 stops being optional.
- **Requests.** Inbox POSTs are Worker requests. On the free plan (100k/day) a lively
  federated space could spend the whole budget on deliveries nobody here asked for. This is
  the one number that argues for federation being a paid-plan feature, or for an inbox that
  answers 202 fast and defers everything to a queue consumer.
- **Threading.** Mastodon replies are flat `inReplyTo` chains; Lemmy comments are trees;
  both map onto materialized paths, but a reply can arrive before its parent (deliveries are
  unordered), and a reply to a post we never received has no parent to hang under. Both need
  handling: fetch the parent, or park the orphan.
- **Deletes and edits from elsewhere.** `Delete` and `Update` arrive for objects we hold;
  honouring them is required, and the tombstone model (§edit.rs) is the right shape.
- **Moderation at instance scale.** A per-instance block list, a way to see which instances
  are sending what, and the willingness to defederate. Spam on the fediverse arrives from
  compromised or throwaway instances in bursts, and the first defence everyone ends up with
  is an allow-list for new instances.
- **Interop is empirical.** Every implementation reads the spec differently. Mastodon wants
  `Note`s with HTML `content`; Lemmy wants `Page`s with `name` and a `Group` that `Announce`s;
  both want `to`/`cc` set just so. There is no test suite; there is running Mastodon and
  Lemmy locally and looking.

### A staged path, if it is ever wanted

**Stage 0 -- now.** Nothing. Reading is public and there are RSS feeds; that is already
"open" in the sense that matters most, and costs nothing.

**Stage 1 -- publish (read-only federation).** WebFinger, actor documents for each space and
user, an outbox, and `Announce`/`Create` deliveries to followers when a thread is posted.
Mastodon users can follow a space and see new threads; nothing comes back. Needs: the actor
table, key generation and storage (a secret per actor; D1 is fine for public keys, private
keys want Secrets Store or KV), signing via `crypto.subtle`, a queue consumer for delivery,
`Follow`/`Accept` handling, which is the one inbound thing. Perhaps two weeks of work and
the moment interop testing starts.

**Stage 2 -- replies come back.** Inbox verification, storing remote actors and replies,
Tier 0 on remote content, orphan handling, `Delete`/`Update`, `Flag` in both directions,
instance blocking in the admin pages. This is where the moderation load and the storage
costs arrive, and where the free tier stops being the target. Several weeks, most of it
edge cases discovered against real servers.

**Stage 3 -- be a Lemmy community.** Group semantics in full: `Announce` every remote reply
to every follower, accept remote *threads* posted to the space, votes as `Like`/`Dislike`.
At that point notespace is a peer of Lemmy and PieFed, which are years into the same work.

### The honest summary

Federation is not a feature to bolt on; it is a second population of users with their own
software, and the cost is mostly the ongoing kind -- moderation, storage, interop breakage
when Mastodon changes something -- rather than the build. Stage 1 is cheap and harmless and
would let people follow a space from Mastodon. Anything past it should wait until there is a
community here whose reach it would extend, because that is the only reason to pay for it.

## 4. Further reading

- ActivityPub, the specification: https://www.w3.org/TR/activitypub/
- ActivityStreams vocabulary (what `Note`, `Create`, `Announce` mean): https://www.w3.org/TR/activitystreams-vocabulary/
- FEP-1b12, how Lemmy-style groups federate: https://codeberg.org/fediverse/fep/src/branch/main/fep/1b12/fep-1b12.md
- Mastodon's implementation notes, the de facto interop reference: https://docs.joinmastodon.org/spec/activitypub/
- Lemmy's federation docs: https://join-lemmy.org/docs/contributors/05-federation.html
- A tutorial-sized implementation ("ActivityPub from scratch"), which is the fastest way to
  see the moving parts: https://blog.joinmastodon.org/2018/06/how-to-implement-a-basic-activitypub-server/
