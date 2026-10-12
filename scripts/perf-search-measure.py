#!/usr/bin/env python3
"""perf-search-measure.py BASE PID

Latency and server CPU for searches over a family name with many matches, one query at a time.
Run by scripts/perf-search.sh, which starts the server and passes its address and process id.
Standard library only."""
import json, subprocess, sys, time, urllib.request

base, pid = sys.argv[1], int(sys.argv[2])

def get(path):
    with urllib.request.urlopen(base + path, timeout=120) as r:
        return json.load(r)

def cputime_seconds(text):
    """`ps -o cputime` is M:SS.cc or H:MM:SS.cc on macOS and [DD-]HH:MM:SS on Linux."""
    days = 0
    if "-" in text:
        day_text, text = text.split("-", 1)
        days = int(day_text)
    seconds = 0.0
    for part in text.split(":"):
        seconds = seconds * 60 + float(part)
    return days * 86400 + seconds

def cpu():
    out = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return cputime_seconds(out)

# The family with the most matches among those of the first few hundred patients.
sample = get("/Patient?_count=300&_total=none")["entry"]
families = sorted({e["resource"]["name"][0]["family"] for e in sample if e["resource"].get("name")})
counts = {f: get(f"/Patient?family={f}&_summary=count")["total"] for f in families}
family = max(counts, key=counts.get)
queries = [
    ("family, page of 10",                f"/Patient?family={family}&_count=10"),
    ("family, page of 10, _total=none",   f"/Patient?family={family}&_count=10&_total=none"),
    ("family, 100 pages deep",            f"/Patient?family={family}&_count=10&_offset=1000"),
    ("family, 100 deep, _total=none",     f"/Patient?family={family}&_count=10&_offset=1000&_total=none"),
    ("family, _summary=count",            f"/Patient?family={family}&_summary=count"),
    ("family + name (checked, all match)", f"/Patient?family={family}&name={family[0]}&_count=10"),
    ("same, _total=none",                 f"/Patient?family={family}&name={family[0]}&_count=10&_total=none"),
    ("family + birthdate (none match)",   f"/Patient?family={family}&birthdate=ge1900-01-01&_count=10"),
    ("no filter, page of 10",             "/Patient?_count=10"),
    ("no filter, _total=none",            "/Patient?_count=10&_total=none"),
]
REPEAT = 20
print(f"family={family!r}")
print(f"{'query':<36}{'matches':>9}{'p50 ms':>9}{'p95 ms':>9}{'CPU ms/q':>10}")
for name, path in queries:
    get(path)  # warm
    lat = []
    c0 = cpu()
    total = None
    for _ in range(REPEAT):
        t = time.perf_counter()
        bundle = get(path)
        lat.append((time.perf_counter() - t) * 1000)
        total = bundle.get("total", "-")
    c1 = cpu()
    lat.sort()
    print(f"{name:<36}{str(total):>9}{lat[len(lat)//2]:>9.1f}{lat[int(len(lat)*0.95)-1]:>9.1f}{(c1-c0)*1000/REPEAT:>10.1f}")
