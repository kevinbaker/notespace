import sqlite3, time, random
db = sqlite3.connect(":memory:")
db.executescript("""
CREATE TABLE space (
  id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
  parent_id INTEGER REFERENCES space(id),
  path TEXT NOT NULL           -- materialized: 'sports/hockey/nhl'
);
CREATE UNIQUE INDEX idx_space_path ON space(path);
CREATE TABLE thread (
  id INTEGER PRIMARY KEY, space_id INTEGER NOT NULL, title TEXT NOT NULL,
  rank REAL NOT NULL, space_path TEXT NOT NULL
);
CREATE INDEX idx_thread_space_rank ON thread(space_id, rank DESC);
CREATE INDEX idx_thread_subtree ON thread(space_path, rank DESC);
""")
# A realistic forum shape: 12 top-level, each with 0-6 children, some grandchildren.
sid = 0; spaces = []
random.seed(7)
for t in range(12):
    sid += 1; top = (sid, f"top{t}", None, f"top{t}"); spaces.append(top)
    for c in range(random.randint(0, 6)):
        sid += 1; ch = (sid, f"top{t}-c{c}", top[0], f"top{t}/c{c}"); spaces.append(ch)
        for g in range(random.randint(0, 3)):
            sid += 1; spaces.append((sid, f"top{t}-c{c}-g{g}", ch[0], f"top{t}/c{c}/g{g}"))
db.executemany("INSERT INTO space(id,slug,name,parent_id,path) VALUES (?,?,?,?,?)",
               [(i, s, s, p, path) for i, s, p, path in spaces])
n = len(spaces)
for i in range(60000):
    sp = spaces[i % n]
    db.execute("INSERT INTO thread(space_id,title,rank,space_path) VALUES (?,?,?,?)",
               (sp[0], f"t{i}", random.random(), sp[3]))
db.commit()
print(f"{n} spaces (3 levels), 60k threads\n")

deep = [s for s in spaces if s[3].count('/') == 2]
target = deep[len(deep)//2]
print(f"resolving /s/{target[3]}  (depth 3)\n")

def timed(label, fn, iters=2000):
    fn()
    t0 = time.perf_counter()
    for _ in range(iters): fn()
    ms = (time.perf_counter()-t0)*1000/iters
    print(f"  {label:<44} {ms*1000:7.1f} us/lookup   {q[0]} quer{'y' if q[0]==1 else 'ies'}")

q=[0]
def walk():
    q[0]=3
    cur=None
    for seg in target[3].split('/'):
        # per-parent slug: one query per level
        r=db.execute("SELECT id FROM space WHERE parent_id IS ? AND path LIKE ?",
                     (cur, f"%{seg}")).fetchone()
        cur=r[0] if r else None
    return cur
def matpath():
    q[0]=1
    return db.execute("SELECT id FROM space WHERE path=?", (target[3],)).fetchone()
def flatslug():
    q[0]=1
    return db.execute("SELECT id FROM space WHERE slug=?", (target[1],)).fetchone()
def wholetree():
    q[0]=1
    rows=db.execute("SELECT id,slug,parent_id,path FROM space").fetchall()
    return {r[3]: r[0] for r in rows}.get(target[3])

print("URL RESOLUTION")
timed("nested walk, one query per level", walk)
timed("materialized path on space", matpath)
timed("flat global slug", flatslug)
timed("load whole tree, resolve in memory", wholetree)

print("\nSUBTREE THREAD LISTING (all threads under a top-level space)")
top = spaces[0]
def own_only():
    q[0]=1
    return db.execute("SELECT id FROM thread WHERE space_id=? ORDER BY rank DESC LIMIT 50",(top[0],)).fetchall()
def subtree_cte():
    q[0]=1
    return db.execute("""
      WITH RECURSIVE d(id) AS (SELECT id FROM space WHERE id=?
        UNION ALL SELECT s.id FROM space s JOIN d ON s.parent_id=d.id)
      SELECT t.id FROM thread t JOIN d ON t.space_id=d.id ORDER BY t.rank DESC LIMIT 50""",(top[0],)).fetchall()
def subtree_prefix():
    q[0]=1
    return db.execute("SELECT id FROM thread WHERE space_path>=? AND space_path<? ORDER BY rank DESC LIMIT 50",
                      (top[3], top[3]+'0')).fetchall()
timed("own threads only (no hierarchy)", own_only, 500)
timed("descendants via recursive CTE", subtree_cte, 500)
timed("descendants via path prefix range", subtree_prefix, 500)

print("\nrows scanned (what D1 actually bills):")
for label, sql, args in [
    ("recursive CTE", """WITH RECURSIVE d(id) AS (SELECT id FROM space WHERE id=?
        UNION ALL SELECT s.id FROM space s JOIN d ON s.parent_id=d.id)
        SELECT t.id FROM thread t JOIN d ON t.space_id=d.id ORDER BY t.rank DESC LIMIT 50""", (top[0],)),
    ("path prefix range", "SELECT id FROM thread WHERE space_path>=? AND space_path<? ORDER BY rank DESC LIMIT 50", (top[3], top[3]+'0')),
]:
    plan = db.execute("EXPLAIN QUERY PLAN "+sql, args).fetchall()
    print(f"  {label}:")
    for p in plan: print(f"      {p[-1]}")
