#!/usr/bin/env python3
"""Summarise a manx trace session pulled from the Pi (deploy/pi-trace.sh).

    tools/pi_trace_report.py traces/pi/latest
    tools/pi_trace_report.py traces/pi/20260923-130501

Prints: session info; min/avg/max of every system.csv column; throttle flags
decoded; kernel errors, each with the manx log lines and system samples of
the seconds before it; manx warnings, errors and panics.
"""
import csv
import math
import re
import sys
from datetime import datetime, timedelta
from pathlib import Path

# vcgencmd get_throttled bits
THROTTLE_BITS = {
    0: "under-voltage NOW", 1: "ARM freq capped NOW", 2: "throttled NOW",
    3: "soft temp limit NOW", 16: "under-voltage occurred",
    17: "ARM freq cap occurred", 18: "throttling occurred",
    19: "soft temp limit occurred",
}
KERNEL_BAD = re.compile(r"error|hang|reset|fault|oom|killed|under-?volt|segfault|timeout", re.I)
LOG_TS = re.compile(r"^\[(\S+Z)\s+(\w+)\s")
WINDOW = timedelta(seconds=5)


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def decode(hexval):
    try:
        v = int(hexval, 16)
    except ValueError:
        return hexval
    flags = [name for bit, name in THROTTLE_BITS.items() if v >> bit & 1]
    return f"{hexval} ({', '.join(flags) or 'clean'})"


def num(v):
    try:
        f = float(v)
        return None if math.isnan(f) else f
    except ValueError:
        return None


def main(d: Path):
    d = d.resolve()
    print(f"== {d.name}")
    session = d / "session.txt"
    if session.exists():
        print(session.read_text().rstrip())
        for line in session.read_text().splitlines():
            if line.startswith("throttled"):
                print("  ->", decode(line.split(":", 1)[1].strip()))

    rows = []
    if (d / "system.csv").exists():
        rows = list(csv.DictReader(open(d / "system.csv")))
    if rows:
        t0, t1 = parse_ts(rows[0]["time"]), parse_ts(rows[-1]["time"])
        print(f"\n== system.csv: {len(rows)} samples over {t1 - t0}")
        print(f"{'':18}{'min':>9}{'avg':>9}{'max':>9}   last")
        for col in rows[0]:
            if col in ("time", "throttled"):
                continue
            vals = [v for v in (num(r[col]) for r in rows) if v is not None]
            if vals:
                print(f"{col:18}{min(vals):9.1f}{sum(vals) / len(vals):9.1f}{max(vals):9.1f}   {rows[-1][col]}")
        seen = []
        for r in rows:
            if not seen or r["throttled"] != seen[-1][1]:
                seen.append((r["time"], r["throttled"]))
        print("throttle flags over time:")
        for t, v in seen:
            print(f"  {t}  {decode(v)}")

    log = []
    if (d / "manx.log").exists():
        for line in open(d / "manx.log", errors="replace"):
            m = LOG_TS.match(line)
            log.append((parse_ts(m.group(1)) if m else None, m.group(2) if m else None, line.rstrip()))

    kern = []
    if (d / "kernel.log").exists():
        for line in open(d / "kernel.log", errors="replace"):
            if KERNEL_BAD.search(line):
                try:
                    kern.append((parse_ts(line.split()[0]), line.rstrip()))
                except ValueError:
                    kern.append((None, line.rstrip()))
    print(f"\n== kernel: {len(kern)} suspicious line(s)")
    for t, line in kern:
        print(" ", line)
        if t is None:
            continue
        before = [l for (lt, _, l) in log if lt and t - WINDOW <= lt <= t]
        if before:
            print(f"    manx log in the {WINDOW.seconds} s before:")
            for l in before[-15:]:
                print("     ", l)
        near = [r for r in rows if t - WINDOW <= parse_ts(r["time"]) <= t + timedelta(seconds=1)]
        for r in near:
            print(f"    {r['time'][11:23]} cpu {r['cpu_pct']}% gpu render {r['gpu_render_pct']}% "
                  f"bin {r['gpu_bin_pct']}% jobs/s {r['gpu_jobs_s']} temp {r['temp_c']} "
                  f"rss {r['manx_rss_mb']}MB v3d {r['v3d_mhz']}MHz")

    bad = [l for (_, lvl, l) in log if lvl in ("WARN", "ERROR")]
    tail = [l for (lt, _, l) in log if lt is None and l.strip()]
    print(f"\n== manx: {len(log)} lines, {len(bad)} WARN/ERROR")
    # One flood of the same message would bury everything else: group by the
    # message with its numbers blanked out, most frequent first.
    groups = {}
    for l in bad:
        key = re.sub(r"-?\d[\d.e+-]*", "#", l.split("] ", 1)[-1])[:110]
        groups.setdefault(key, [0, l])[0] += 1
    for key, (n, example) in sorted(groups.items(), key=lambda kv: -kv[1][0])[:15]:
        print(f"  {n:6}x  {key}")
        if n > 1:
            print(f"           e.g. {example.split('] ', 1)[-1][:140]}")
    if tail:
        print("unstructured output (panics, backtraces, driver messages):")
        for l in tail[-30:]:
            print(" ", l)


if __name__ == "__main__":
    main(Path(sys.argv[1] if len(sys.argv) > 1 else "traces/pi/latest"))
