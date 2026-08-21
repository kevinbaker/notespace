import sqlite3
db = sqlite3.connect(":memory:")
db.executescript("CREATE TABLE t(id INTEGER PRIMARY KEY, p TEXT NOT NULL); CREATE INDEX i ON t(p);")

# 'sports' with a child, plus a SIBLING whose slug starts with the same letters.
# '-' is 0x2D and sorts BELOW '/' (0x2F) -- that is the whole trap.
rows_notrail = ["sports", "sports/hockey", "sports/hockey/nhl", "sports-betting", "sportswear", "music"]
rows_trail   = [r + "/" for r in rows_notrail]

def check(label, rows, lo, hi, want):
    db.execute("DELETE FROM t")
    db.executemany("INSERT INTO t(p) VALUES (?)", [(r,) for r in rows])
    got = [r[0] for r in db.execute("SELECT p FROM t WHERE p>=? AND p<? ORDER BY p", (lo, hi))]
    ok = got == want
    print(f"  {label}")
    print(f"    range [{lo!r}, {hi!r})")
    print(f"    got   {got}")
    print(f"    {'PASS' if ok else 'FAIL -- ' + repr([g for g in got if g not in want]) + ' leaked in'}\n")
    return ok

print("\nNo trailing separator (what I measured above):\n")
check("subtree of 'sports'", rows_notrail, "sports", "sports0",
      ["sports", "sports/hockey", "sports/hockey/nhl"])

print("With a trailing separator on every stored path:\n")
check("subtree of 'sports/'", rows_trail, "sports/", "sports0",
      ["sports/", "sports/hockey/", "sports/hockey/nhl/"])

print("Character ordering that makes it work:")
for c in "-/0_azA":
    print(f"    {c!r:>5}  0x{ord(c):02X}")
print("\n  A slug may contain '-' (0x2D), which sorts BELOW '/' (0x2F). Without the trailing")
print("  separator, 'sports-betting' falls inside ['sports','sports0') and leaks into the")
print("  subtree. With it, the range starts at 'sports/' and 'sports-betting/' sorts below it.")
