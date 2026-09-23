#!/usr/bin/env python3
"""Sample the Pi once a second while navcore runs; one CSV row per sample.

Runs on the Pi, started by pi-trace.sh:  pi_sampler.py <navcore-pid> <out.csv>
Exits when navcore does. Each row is fsync'd, so a GPU hang that takes the
desktop down (or a hard lock) still leaves everything up to the last second.

GPU busy % comes from the v3d driver's cumulative per-queue runtimes
(/sys/class/drm/card0/device/gpu_stats); 'render' is the fragment work,
'bin' the vertex/tiling pass.
"""
import os
import subprocess
import sys
import time
from datetime import datetime, timezone

PID = int(sys.argv[1])
OUT = sys.argv[2]
GPU_STATS = "/sys/class/drm/card0/device/gpu_stats"
TICK = os.sysconf("SC_CLK_TCK")
PAGE_MB = os.sysconf("SC_PAGE_SIZE") / 1e6

COLUMNS = [
    "time", "cpu_pct", "load1", "mem_avail_mb", "zram_used_mb",
    "navcore_cpu_pct", "navcore_rss_mb", "navcore_threads",
    "gpu_render_pct", "gpu_bin_pct", "gpu_jobs_s",
    "temp_c", "arm_mhz", "v3d_mhz", "core_v", "ext5v_v", "throttled",
]


def vc(*args):
    try:
        return subprocess.run(["vcgencmd", *args], capture_output=True,
                              text=True, timeout=2).stdout.strip()
    except Exception:
        return ""


def after_eq(s, strip=""):
    try:
        return float(s.split("=", 1)[1].rstrip(strip))
    except (IndexError, ValueError):
        return float("nan")


def cpu_times():
    f = open("/proc/stat").readline().split()[1:]
    vals = list(map(int, f))
    idle = vals[3] + vals[4]
    return sum(vals), idle


def proc_stat(pid):
    try:
        f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    except FileNotFoundError:
        return None
    # fields after the ')' of comm, from state: utime=11, stime=12, threads=17, rss=21
    return int(f[11]) + int(f[12]), int(f[17]), int(f[21]) * PAGE_MB


def gpu():
    out = {}
    try:
        for line in open(GPU_STATS).read().splitlines()[1:]:
            q, ts, jobs, runtime = line.split()
            out[q] = (int(ts), int(jobs), int(runtime))
    except (FileNotFoundError, ValueError):
        pass
    return out


def meminfo():
    m = {}
    for line in open("/proc/meminfo"):
        k, v = line.split(":", 1)
        m[k] = int(v.split()[0])
    return m


def main():
    new = not os.path.exists(OUT)
    fh = open(OUT, "a", buffering=1)
    if new:
        fh.write(",".join(COLUMNS) + "\n")
    prev_cpu, prev_p, prev_g, prev_t = cpu_times(), proc_stat(PID), gpu(), time.monotonic()
    while True:
        time.sleep(1)
        p = proc_stat(PID)
        if p is None:
            break
        now = time.monotonic()
        dt = now - prev_t
        tot, idle = cpu_times()
        dtot, didle = tot - prev_cpu[0], idle - prev_cpu[1]
        cpu_pct = 100 * (1 - didle / dtot) if dtot else 0
        nav_cpu = 100 * (p[0] - prev_p[0]) / TICK / dt if prev_p else 0
        g = gpu()

        def busy(q):
            if q not in g or q not in prev_g:
                return float("nan")
            dts = g[q][0] - prev_g[q][0]
            return 100 * (g[q][2] - prev_g[q][2]) / dts if dts else 0

        jobs = (g["render"][1] - prev_g["render"][1]) / dt if "render" in g and "render" in prev_g else float("nan")
        m = meminfo()
        zram = (m.get("SwapTotal", 0) - m.get("SwapFree", 0)) / 1024
        row = [
            datetime.now(timezone.utc).isoformat(timespec="milliseconds"),
            f"{cpu_pct:.1f}", open("/proc/loadavg").read().split()[0],
            f"{m['MemAvailable'] / 1024:.0f}", f"{zram:.0f}",
            f"{nav_cpu:.1f}", f"{p[2]:.0f}", str(p[1]),
            f"{busy('render'):.1f}", f"{busy('bin'):.1f}", f"{jobs:.0f}",
            f"{int(open('/sys/class/thermal/thermal_zone0/temp').read()) / 1000:.1f}",
            f"{after_eq(vc('measure_clock', 'arm')) / 1e6:.0f}",
            f"{after_eq(vc('measure_clock', 'v3d')) / 1e6:.0f}",
            f"{after_eq(vc('measure_volts', 'core'), 'V'):.3f}",
            f"{after_eq(vc('pmic_read_adc', 'EXT5V_V'), 'V'):.3f}",
            vc("get_throttled").split("=")[-1],
        ]
        fh.write(",".join(row) + "\n")
        os.fsync(fh.fileno())
        prev_cpu, prev_p, prev_g, prev_t = (tot, idle), p, g, now


if __name__ == "__main__":
    main()
