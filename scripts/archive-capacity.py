import sqlite3, os, time, random, secrets
C = "0123456789abcdefghjkmnpqrstvwxyz"
def b32(n,ch):
    o=[]
    for _ in range(ch): o.append(C[n&31]); n>>=5
    return "".join(reversed(o))
N=200_000; base_ms=1735689600000; CEILING=500*1024*1024
def mkid(i,w):
    if w==16: return b32(((base_ms+i)<<32)|secrets.randbits(32),16)
    return b32(((((base_ms+i)<<80)|secrets.randbits(80))<<2),26)

def build(path,width,bodies):
    if os.path.exists(path): os.remove(path)
    db=sqlite3.connect(path); db.executescript("PRAGMA journal_mode=OFF;PRAGMA synchronous=OFF;")
    body_cols = "body_md TEXT NOT NULL, body_html TEXT NOT NULL," if bodies else ""
    pid_col   = "public_id TEXT," if width else ""
    db.executescript(f"""
    CREATE TABLE post (
      id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL, parent_id INTEGER,
      path TEXT NOT NULL, depth INTEGER NOT NULL, author_id INTEGER NOT NULL,
      {pid_col}{body_cols}
      created_at INTEGER NOT NULL, edited_at INTEGER, score REAL NOT NULL DEFAULT 0,
      state TEXT NOT NULL DEFAULT 'visible');
    CREATE UNIQUE INDEX idx_post_thread_path ON post(thread_id, path);
    CREATE INDEX idx_post_author ON post(author_id, created_at DESC);
    """)
    if width: db.execute("CREATE UNIQUE INDEX idx_post_public_id ON post(public_id)")
    cols=["id","thread_id","path","depth","author_id"]
    if width: cols.append("public_id")
    if bodies: cols += ["body_md","body_html"]
    cols.append("created_at")
    rows=[]
    for i in range(N):
        r=[i+1,i//50,f"{i%50:04d}",0,1]
        if width: r.append(mkid(i,width))
        if bodies: r += ["x"*400]*2
        r.append(base_ms+i)
        rows.append(tuple(r))
    db.executemany(f"INSERT INTO post({','.join(cols)}) VALUES ({','.join('?'*len(cols))})",rows)
    db.commit(); db.executescript("ANALYZE;")
    size=os.path.getsize(path); db.close(); return size

print(f"{N:,} posts. Two worlds: bodies in D1, and bodies archived to R2.\n")
for bodies,label in [(True,"bodies IN D1"),(False,"bodies in R2 (metadata only)")]:
    base=build(f"/tmp/pid/a_{bodies}_0.db",None,bodies)
    print(f"=== {label} ===")
    pb=base/N; capb=CEILING/pb
    print(f"  {'mode':<14} {'B/post':>7} {'posts in 500MB':>16} {'overhead':>10} {'cost at ceiling':>17}")
    print(f"  {'none':<14} {pb:>7.0f} {capb:>16,.0f} {'—':>10} {'—':>17}")
    for w in (16,26):
        s=build(f"/tmp/pid/a_{bodies}_{w}.db",w,bodies)
        pp=s/N; over=(s-base)/N; cap=CEILING/pp
        cost=capb*over/1024/1024
        print(f"  {str(w)+'-char':<14} {pp:>7.0f} {cap:>16,.0f} {'+'+str(round(over))+' B':>10} {f'{cost:.0f} MB / {over/pb*100:.1f}%':>17}")
    print()

# Sparse revisited: I dropped it because it only saved 2.6%. In the archived world it is
# saving ~53%, which is a different argument entirely.
def build_sparse(path,width,rate):
    if os.path.exists(path): os.remove(path)
    db=sqlite3.connect(path); db.executescript("PRAGMA journal_mode=OFF;PRAGMA synchronous=OFF;")
    db.executescript("""
    CREATE TABLE post (
      id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL, parent_id INTEGER,
      path TEXT NOT NULL, depth INTEGER NOT NULL, author_id INTEGER NOT NULL,
      public_id TEXT,
      created_at INTEGER NOT NULL, edited_at INTEGER, score REAL NOT NULL DEFAULT 0,
      state TEXT NOT NULL DEFAULT 'visible');
    CREATE UNIQUE INDEX idx_post_thread_path ON post(thread_id, path);
    CREATE INDEX idx_post_author ON post(author_id, created_at DESC);
    CREATE UNIQUE INDEX idx_post_public_id ON post(public_id) WHERE public_id IS NOT NULL;
    """)
    random.seed(11)
    rows=[(i+1,i//50,f"{i%50:04d}",0,1, mkid(i,width) if random.random()<rate else None, base_ms+i)
          for i in range(N)]
    db.executemany("INSERT INTO post(id,thread_id,path,depth,author_id,public_id,created_at) VALUES (?,?,?,?,?,?,?)",rows)
    db.commit(); db.executescript("ANALYZE;")
    s=os.path.getsize(path); db.close(); return s

base=build(f"/tmp/pid/sp_base.db",None,False)
pb=base/N; capb=CEILING/pb
print("=== bodies in R2, sparse ids (only posts someone actually linked) ===")
print(f"  {'mode':<22} {'B/post':>7} {'posts in 500MB':>16} {'overhead':>10} {'vs none':>9}")
print(f"  {'none':<22} {pb:>7.0f} {capb:>16,.0f} {'—':>10} {'—':>9}")
for w in (16,26):
    for rate,tag in [(1.0,"100%"),(0.05,"5%"),(0.01,"1%")]:
        s = build_sparse(f"/tmp/pid/sp_{w}_{rate}.db",w,rate)
        pp=s/N; over=(s-base)/N; cap=CEILING/pp
        print(f"  {f'{w}-char, {tag} have one':<22} {pp:>7.0f} {cap:>16,.0f} {'+'+str(round(over))+' B':>10} {f'-{(1-cap/capb)*100:.0f}%':>9}")
