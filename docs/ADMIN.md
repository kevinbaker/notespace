# Running a notespace

For the people who run one: the first admin, spaces, the moderation pipeline and the queue,
accounts, and what every setting does. Getting it deployed is [DEPLOY.md](DEPLOY.md).

## Who can do what

| | member | moderator | admin |
|---|---|---|---|
| Read, post, reply, edit and delete their own posts, report, appeal | ✓ | ✓ | ✓ |
| Work the review queue: approve or reject held posts | | ✓ | ✓ |
| Restore, hide, delete any post; send one back to the queue | | ✓ | ✓ |
| Pin, lock, hide, delete, retitle or move a thread | | ✓ | ✓ |
| Ban an account; delete an account | | ✓ | ✓ |
| Change anyone's role | | | ✓ |
| Create and configure spaces, including their moderation policy and theme | | | ✓ |
| Read the full action log | | ✓ | ✓ |

Nobody changes their own account from the admin pages -- not its role, not its state. An admin
who demotes themself would be an instance with no admin.

### The first admin

Nobody has a role in a fresh database. `MODERATORS` (a variable: `wrangler.toml` on Cloudflare,
the environment when self-hosted) names one or more accounts, comma-separated, that act as
admin the moment they sign in -- before they have the role, before the account even exists.
Register under that name and you are in.

The bootstrap is per request, not stored: the account's role in the database is still
"member". To make it permanent, grant a second account admin from `/admin/user/<name>`, sign in
as that one, and grant the first. After that `MODERATORS` can be emptied, or left as a spare
key under the mat.

## The admin pages

Everything is under `/admin`, linked from the header once you are signed in as a moderator
("queue") and from the account page. Every action is a form with a CSRF token; there is no API.

| page | what is there |
|---|---|
| `/admin` | Counts (users, threads, posts, pending, open reviews, banned) and the recent threads. |
| `/admin/users` | Search by the start of a name. A user's page shows email and its confirmation, and offers **state** (active, ban, delete account) and, to admins, **role**. |
| `/admin/spaces` | Every space with its thread count and policy summary; the form to create one. |
| `/admin/space/<id>` | One space: name, nesting, ranking, moderation policy, theme. |
| `/admin/thread/<id>` | Title, link, state, which space it is in; then every post in the thread with its state and the buttons to change it. `<id>` is the thread's public id, the one in its URL. |
| `/admin/log` | Every action anyone or anything has taken, with the detail the public log leaves out. |
| `/mod/queue` | Held posts waiting for a decision. |
| `/modlog` | The public log: actions and targets, no rationale, no reporter. |

### Accounts

**Ban** ends every session the account has at once and refuses new ones. The name stays taken;
posts stay as they are. **Delete account** keeps the name reserved (a returning troll cannot
re-register it) and turns every post into a `[deleted]` tombstone so replies keep their
context. Both are reversible from the same page: set the state back to active. Members cannot
delete their own accounts in this release; do it for them on request.

**Roles** are member, moderator, admin, as in the table. A ban on a moderator works like any
other ban.

### Threads and posts

A thread has one of five states:

| state | listed? | readable? | replies? |
|---|---|---|---|
| visible | ✓ | ✓ | ✓ |
| pinned | ✓, first | ✓ | ✓ |
| locked | ✓ | ✓ | no -- "locked: no new replies" |
| hidden | no | 404 | no |
| deleted | no | 404 | no |

Hiding is reversible and lifts at once (the 404 is not cached). Moving a thread to another
space keeps every link to it working: threads and posts are addressed by ids, never by space.
Retitling likewise -- the slug in `/t/<id>/<slug>` is decorative.

A post has one of four states, and readers see the text of only the first:

| state | readers see |
|---|---|
| visible | the post |
| pending | `[awaiting review]` -- held, waiting for the classifier or the queue |
| hidden | `[removed by moderator]` |
| deleted | `[deleted]` -- the author's own doing, or an account deletion |

From the thread's admin page each post has **restore**, **hide**, **send to queue** (back to
pending, with a review item) and **delete**. Moderators can delete anyone's post but edit only
their own; "edit text" on someone else's post lands on a page that says so. Authors edit their
own posts freely; edited text goes back through the heuristics like a new post.

## Spaces

A space is a board with its own URL, nesting depth, moderation policy and look. Spaces nest
three deep: `/s/sports`, `/s/sports/hockey`, `/s/sports/hockey/goalies`, and no further. A
space page lists the threads of every space beneath it too, so `/s/sports` is a section page.

**Key** is the URL segment: 2--32 characters, letters, digits, `-` and `_`, not starting or
ending with a separator, at least one letter, and not one of the reserved words (`admin`,
`api`, `new`, `rss`, `settings`, …). Unique under its parent, so `sports/general` and
`music/general` can coexist. The key is fixed once created; the display **name** is not.

**Reply nesting depth**: how deep replies indent before the page flattens them. `0` is a flat
board (a classic bulletin board, every reply at the same level); `8` is a Reddit-style tree.
Replies deeper than the cap are refused with "reply higher up the thread". Changing it changes
the rendering of existing threads, not their structure.

**Ranking**: `bump` (most recent activity first, pinned threads on top) is what this release
implements. The other options are stored and reserved for when scoring exists; today they
render the same as `bump`.

### Moderation policy

Every post goes through the same pipeline, and the policy is what tunes it for a space. The
defaults are conservative; a beta where every account is new will want `new_account_hours`
lowered on every space, or every first post will wait.

| setting | default | what it does |
|---|---|---|
| Moderation on | on | Off means every post publishes immediately and the classifier never runs. A post held while it was on publishes the next time the sweep sees it. |
| Hold every post from accounts younger than | 72 h | The main gate. `0` never holds for age. |
| Links per post before holding (established accounts) | 5 | Over this, held. |
| … for new accounts | 1 | Same, for accounts inside the age window. A new account posting two links is the shape of most spam. |
| Reports needed to pull a post back for review | 3 | Distinct reporters. At the threshold the post goes back to pending and into the queue, and the classifier gets a second look. |
| Refuse an identical post from the same author within | 24 h | A duplicate body is refused outright, not held. `0` disables. |
| Publish without a human when the model says clean at confidence ≥ | 0.75 | The auto-publish bar. Lower means more posts skip the queue; the dangerous direction. |
| Hide pending review when the model flags at confidence ≥ | 0.90 | A confident flag for a *severe* category hides the post until a human confirms. Below it, the post stays held and waits. |
| Blocklist | empty | One term per line, case-insensitive. A match holds the post with the term named in the log. |
| Space rules, in prose | empty | Handed to the classifier as the space's rules, so "no politics" means something to the model. Also worth writing for people; it is not shown to them yet. |
| Public moderation log | on | Reserved. Every action is in `/modlog` in this release regardless. |

What "held" means, step by step:

1. **Tier 0, free and instant, on every post and edit.** Account age, link count, blocklist,
   duplicate body, text addressed to the model rather than to readers ("ignore previous
   instructions…"), and shouting (mostly upper case past a length where that is a choice).
   Anything caught is written as **pending**, logged as "held for review" with its reasons, and
   handed to the classifier. Nothing else happens on the request; the author sees "your reply
   is waiting for a quick check" with a link to it.
2. **The classifier**, if one is configured (`MOD_PROVIDER`; see DEPLOY.md). It reads the post,
   the thread title, the space's rules and the reasons Tier 0 gave, and answers *clean*, *flag*
   or *unsure* with a confidence and categories. Then:
   - **clean** at or above the publish threshold → published, logged as the model's action;
   - **flag** in a severe category (spam, harassment, hate, violence, sexual content,
     self-harm, illegal, doxxing) at or above the hide threshold → **hidden**, and into the
     queue for a human to confirm;
   - anything else -- unsure, a low-confidence call, "off topic", or *manipulation*, which is
     never auto-published however confident → stays pending, into the queue.
3. **No classifier, or one that failed** (out of budget, unreachable) → into the queue marked
   "classifier unavailable". Nothing waits on a machine that is not answering.
4. **The sweep**, every five minutes, picks up anything pending for more than a minute that
   the queue has not handled, so a lost message or a dead queue costs a wait, not a post.

### Metamoderation

Every queue decision is also a grade for the model: approving what it flagged, or rejecting
what it called clean, is a disagreement. Per space, once ten reviews have been resolved, a
disagreement rate above 30% raises the publish threshold by 0.15 and the hide threshold by
0.05 -- the model is trusted less, more goes to humans. Once thirty have been resolved with
under 5% disagreement, the publish threshold drops by 0.05, to a floor of 0.5. The space form
shows the configured thresholds; the effective ones are those plus this adjustment, and the
log's "classify" rows carry the confidence each verdict came with.

### Theme

Two fields at the bottom of the space form, both optional, both validated on save.

**Overrides** are the stylesheet's tokens, one `name: value` per line: `bg`, `bg-2`, `fg`,
`fg-2`, `line`, `accent`, `on-accent`, `danger`, `warn`, `font`, `mono`, `size`, `lh`,
`radius`, `measure`, `indent`. Prefix a name with `light.` or `dark.` to set it for one colour
scheme. Values are colours, lengths, keywords and quoted font stacks; `url()` and anything else
that fetches is refused here.

```
accent: #1d4ed8
dark.accent: #7aa2ff
font: "IBM Plex Sans", system-ui, sans-serif
measure: 52rem
```

**Stylesheet** is the space's own CSS, applied after the site's, written against the site's
class names (`.site`, `.threads`, `.post`, `.post-body`, …) and tokens (`var(--accent)`).
Up to 16 KB; `@import` and `<` are refused; `url()` is allowed, so a banner works -- and so
does an image that logs who loads it, which is why only admins edit themes. Both are served as
one immutable stylesheet per space; a change is live on the next page load.

The site as a whole has the same two things in `SITE_THEME` (DEPLOY.md, "The look"). A space's
theme layers over the site's.

## The review queue

`/mod/queue` lists every open item, oldest first. Each shows the post with its text (a reviewer
sees hidden text; that is the point), who wrote it and how old the account is, which thread and
space, **why it is here** -- held by the classifier, reported by readers, held by a rule, an
appeal from the author, or classifier unavailable -- the model's verdict and confidence if it
gave one, and the author's appeal text if there is one. Two buttons:

- **Approve** publishes the post (state → visible).
- **Reject** hides it (state → hidden). The author sees `[removed by moderator]` and may appeal
  once from the post's page.

Either resolves the item and logs the decision under the moderator's name, in the public log
too. An item another moderator resolved in the meantime says so rather than acting twice.

**Reports** come from readers at `/p/<id>/report` with an optional reason (500 characters); the
same reader reporting twice counts once. Who reported is in the full log, never the public one.

**Appeals** come from the author of a hidden post at `/p/<id>/appeal` with their text (2,000
characters). The item reopens with the text attached and the original verdict still on it.

## Keeping the door

`SIGNUP_CODE` (a secret) makes `/register` require an invite code; the site stays readable by
everyone and writable by people who were given the word. Remove the secret to open
registration. Sign-in through Google or GitHub is not gated by it.

Rate limits are built in: five failed sign-ins per account or sixty per address in fifteen
minutes, and per-author and per-address limits on new threads and replies, all answered with
"try again in about N minutes". Cloudflare's own WAF rules and Access are the layer in front
for a closed beta; DEPLOY.md has the recipe.

## What the logs say

`/admin/log` has everything: who (a person, a rule, the model, or the system), what they did
(hold, classify, publish, hide, approve, reject, report, appeal, configure, ban, …), to what,
when, and the detail -- the reasons a rule held for, the model's confidence and categories, the
count of reports, the setting that changed. `/modlog` is the public subset: no rationale, no
reporter, but every action by anyone on any post, so a community can see its moderation
happening. There is no way to edit or delete a log row.

## Everyday

- **A first post from a new member is stuck "awaiting review".** That is `new_account_hours`
  doing its job; approve it in the queue, or lower the setting for the space.
- **A member says they cannot reply.** The thread is locked, the space's nesting cap is
  reached ("reply higher up"), they are rate-limited, or the account is banned. The message
  they saw says which.
- **Spam got through.** Hide it from the thread's admin page, ban the account, add the term to
  the blocklist, and consider lowering `max_links_new` to `0` for a while.
- **The model is too eager.** Watch the queue: if you are approving most of what it flags,
  metamoderation will raise its thresholds on its own after ten decisions. To act sooner, raise
  `hide_confidence` yourself, or switch providers (DEPLOY.md).
- **Someone wants their account gone.** Delete it from their user page: the name stays
  reserved, the posts become tombstones. Their email address is not removed from the row; if
  that matters where you are, note it.
- **You locked yourself out.** `MODERATORS` in the configuration is always an admin; set it to
  your name and sign in.
