#!/usr/bin/env python3
"""Write through a load balancer while a primary is killed, then read every acknowledged write back.

  failover_load.py write  BASE SECONDS LABEL   > results.json   (writes a Patient every 100 ms)
  failover_load.py verify BASE results.json FAIL_BUDGET         (exit 1 if anything is wrong)

Used by deploy/cluster/failover-test.sh (Compose) and deploy/helm/refhir/kind-test.sh (Kubernetes).
`verify` checks that every write acknowledged with a 2xx can be read afterwards, asking for at least
the commit timestamp it was acknowledged with (X-Refhir-Min-Ts), as the clients do; that the commit
timestamps of consecutive acknowledged writes only increase; and that at most FAIL_BUDGET requests
failed (a request in flight on the killed primary may). Standard library only.
"""
import json
import sys
import time
import urllib.error
import urllib.request
import uuid


def write(base, seconds, label):
    label = "".join(c for c in label if c.isalnum())  # a FHIR id has no spaces
    acked, failed = [], []
    last_ack, longest_gap, gap_at = None, 0.0, 0.0
    begin = time.time()
    end = begin + seconds
    n = 0
    while time.time() < end:
        n += 1
        pid = f"{label}-{n}-{uuid.uuid4().hex[:6]}"
        body = json.dumps({"resourceType": "Patient", "id": pid, "active": True}).encode()
        req = urllib.request.Request(
            f"{base}/Patient", data=body, method="POST",
            headers={"content-type": "application/fhir+json"},
        )
        started = time.time()
        try:
            with urllib.request.urlopen(req, timeout=20) as resp:
                ts = resp.headers.get("X-Refhir-Commit-Ts")
                now = time.time()
                if last_ack is not None and now - last_ack > longest_gap:
                    longest_gap, gap_at = now - last_ack, now - begin
                last_ack = now
                acked.append({"id": pid, "ts": int(ts) if ts else None})
        except urllib.error.HTTPError as err:
            failed.append({"id": pid, "status": err.code, "at": round(started - begin, 2)})
        except Exception as err:  # connection reset, timeout
            failed.append({"id": pid, "status": str(err)[:60], "at": round(started - begin, 2)})
        time.sleep(0.1)
    print(json.dumps({
        "acked": acked, "failed": failed,
        "longest_gap_s": round(longest_gap, 2), "gap_at_s": round(gap_at, 1),
    }))


def verify(base, path, budget):
    result = json.load(open(path))
    acked, failed = result["acked"], result["failed"]
    problems = []
    stamped = [a["ts"] for a in acked if a["ts"] is not None]
    if len(stamped) != len(acked):
        problems.append(f"{len(acked) - len(stamped)} acknowledged writes carried no X-Refhir-Commit-Ts")
    backwards = sum(1 for a, b in zip(stamped, stamped[1:]) if b <= a)
    if backwards:
        problems.append(f"{backwards} acknowledged writes got a commit timestamp not above the one before")
    missing = []
    for a in acked:
        headers = {"X-Refhir-Min-Ts": str(a["ts"])} if a["ts"] else {}
        deadline = time.time() + 20
        while True:
            try:
                req = urllib.request.Request(f"{base}/Patient/{a['id']}", headers=headers)
                with urllib.request.urlopen(req, timeout=10) as resp:
                    if resp.status == 200:
                        break
            except Exception:
                pass
            if time.time() > deadline:
                missing.append(a["id"])
                break
            time.sleep(0.2)
    if missing:
        problems.append(f"{len(missing)} acknowledged writes cannot be read: {missing[:5]}")
    if len(failed) > budget:
        problems.append(f"{len(failed)} requests failed (budget {budget})")
    print(f"    {len(acked)} writes acknowledged, {len(failed)} failed "
          f"{[(f['at'], f['status']) for f in failed]}, "
          f"longest gap between acknowledgements {result['longest_gap_s']}s (at t={result['gap_at_s']}s)")
    print(f"    timestamps strictly increasing: {not backwards}; acknowledged writes missing: {len(missing)}")
    if problems:
        for problem in problems:
            print("    FAIL:", problem)
        sys.exit(1)


if __name__ == "__main__":
    if len(sys.argv) == 5 and sys.argv[1] == "write":
        write(sys.argv[2], float(sys.argv[3]), sys.argv[4])
    elif len(sys.argv) == 5 and sys.argv[1] == "verify":
        verify(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else:
        sys.exit(__doc__)
