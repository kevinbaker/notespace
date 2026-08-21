import sqlite3, os, time, random, secrets

C = "0123456789abcdefghjkmnpqrstvwxyz"
def b32(n, ch):
    o=[]
    for _ in range(ch): o.append(C[n & 31]); n >>= 5
    return "".join(reversed(o))

N, BODY = 200_000, 400
base_ms = 1735689600000
CEILING = 500*1024*1024

def make_id(i, width):
    if width == 16:
        return b32(((base_ms+i) << 32) | secrets.randbits(32), 16)
    # 26 chars = 130 bits of space, 128 of payload, low 2 bits reserved zero
    return b32((((base_ms+i) << 80) | secrets.randbits(80)) << 2, 26)

def build(path, width, sparse=None):
    if os.path.exists(path): os.remove(path)
    db = sqlite3.connect(path)
    db.executescript("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")
    pid_col = "public_id TEXT," if width else ""
    db.executescript(f"""
    CREATE TABLE post (
      id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL, parent_id INTEGER,
      path TEXT NOT NULL, depth INTEGER NOT NULL, author_id INTEGER NOT NULL,
      {pid_col}
      body_md TEXT NOT NULL, body_html TEXT NOT NULL,
      created_at INTEGER NOT NULL, score REAL NOT NULL DEFAULT 0,
      state TEXT NOT NULL DEFAULT 'visible');
    CREATE UNIQUE INDEX idx_post_thread_path ON post(thread_id, path);
    CREATE INDEX idx_post_author ON post(author_id, created_at DESC);
    """)
    if width:
        where = " WHERE public_id IS NOT NULL" if sparse else ""
        db.execute(f"CREATE UNIQUE INDEX idx_post_public_id ON post(public_id){where}")
    random.seed(11)
    rows=[]
    for i in range(N):
        body = "x"*BODY
        pid = None
        if width and (sparse is None or random.random() < sparse):
            pid = make_id(i, width)
        rows.append((i+1, i//50, f"{i%50:04d}", 0, 1, pid, body, body, base_ms+i) if width
                    else (i+1, i//50, f"{i%50:04d}", 0, 1, body, body, base_ms+i))
    cols = "id,thread_id,path,depth,author_id,public_id,body_md,body_html,created_at" if width \
           else "id,thread_id,path,depth,author_id,body_md,body_html,created_at"
    q = "?,"*(len(cols.split(","))-1)+"?"
    t0=time.perf_counter()
    db.executemany(f"INSERT INTO post({cols}) VALUES ({q})", rows)
    db.commit()
    ins=(time.perf_counter()-t0)*1000
    db.executescript("ANALYZE;")
    return db, ins, os.path.getsize(path)

def idx_mb(db,name):
    try: return (db.execute("SELECT SUM(pgsize) FROM dbstat WHERE name=?", (name,)).fetchone()[0] or 0)/1024/1024
    except sqlite3.OperationalError: return 0

MODES = [
    ("none",            None, None),
    ("16-char full",    16,   None),
    ("26-char full",    26,   None),
    ("16-char sparse",  16,   0.01),
    ("26-char sparse",  26,   0.01),
]
print(f"{N:,} posts, ~{BODY}B body stored twice, 50 posts/thread. sparse = 1% populated.\n")
res={}
for label,width,sparse in MODES:
    db,ins,size = build(f"/tmp/pid/w{width}{'s' if sparse else ''}.db", width, sparse)
    def page():
        return db.execute("SELECT id,path,body_html FROM post WHERE thread_id=? AND path>'' ORDER BY path LIMIT 201",(random.randrange(N//50),)).fetchall()
    page()
    t0=time.perf_counter()
    for _ in range(2000): page()
    read=(time.perf_counter()-t0)*1e6/2000
    res[label]=(size, idx_mb(db,"idx_post_public_id"), ins, read)
    db.close()

base=res["none"][0]
per_post_base = base/N
print(f"{'mode':<16} {'db size':>10} {'vs none':>9} {'pid index':>10} {'insert':>9} {'page read':>10}")
print("-"*70)
for label,_,_ in MODES:
    size,pidx,ins,read = res[label]
    d = f"+{(size-base)/1024/1024:.1f} MB" if size>base else "—"
    print(f"{label:<16} {size/1024/1024:>7.1f} MB {d:>9} {pidx:>7.2f} MB {ins:>6.0f} ms {read:>7.1f} us")

print(f"\n{'mode':<16} {'B/post':>8} {'posts in 500MB':>16} {'vs none':>10} {'ceiling cost':>13}")
print("-"*70)
cap_base = CEILING/per_post_base
for label,_,_ in MODES:
    size = res[label][0]
    pp = size/N
    cap = CEILING/pp
    over = (size-base)/N
    cost = cap_base*over/1024/1024
    print(f"{label:<16} {pp:>8.0f} {cap:>16,.0f} {('—' if label=='none' else f'-{(1-cap/cap_base)*100:.1f}%'):>10} {('—' if label=='none' else f'{cost:.1f} MB / {cost/500*100:.1f}%'):>13}")
