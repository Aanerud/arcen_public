#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Arcen real-screen latency harness (Mac side).

Runs the real Arcen Deck GUI against a real Pier, opens the latency page in a
real browser inside the host desktop, drives the Deck window with real macOS
input (realscreen-driver), and reports what reached the glass and when.

  realscreen.py run --ssh root@HOST --port 18444 --pin-file host.pin \
      --deck-app "/path/Arcen Deck.app" --pipeline auto --device mouse \
      --label x11-prod-auto-mouse --out results/
  realscreen.py analyze <run-dir>      # re-analyse a finished run

Credentials come from a local file (default ~/.arcen-nightly.env) holding a
line "<label> : <user> / <password>"; they go to the Deck on stdin
(--credentials-stdin) and are never written anywhere. See
docs/testing/realscreen.md.
"""
import argparse
import bisect
import datetime as dt
import json
import os
import pathlib
import re
import shlex
import signal
import statistics
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[2]
DRIVER_SRC = HERE / "driver.swift"
DRIVER_BIN = ROOT / "target" / "realscreen" / "realscreen-driver"
HOST_FILES = ("latency.html", "page_server.py", "host_page.sh")


def log(msg):
    print(f"[realscreen {time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)


def ssh(target, command, timeout=120, check=False):
    proc = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", target, command],
        capture_output=True, text=True, timeout=timeout)
    if check and proc.returncode != 0:
        raise RuntimeError(f"ssh {command[:60]!r} failed: {proc.stderr.strip()[-400:]}")
    return proc


def build_driver():
    if DRIVER_BIN.exists() and DRIVER_BIN.stat().st_mtime >= DRIVER_SRC.stat().st_mtime:
        return DRIVER_BIN
    DRIVER_BIN.parent.mkdir(parents=True, exist_ok=True)
    log("building realscreen-driver")
    subprocess.run(["swiftc", "-O", "-swift-version", "5", str(DRIVER_SRC), "-o", str(DRIVER_BIN)],
                   check=True, capture_output=True)
    return DRIVER_BIN


def read_credentials(path, label):
    pattern = re.compile(r"^\s*" + re.escape(label) + r"\s*:\s*(\S+)\s*/\s*(.+?)\s*$")
    for line in pathlib.Path(path).expanduser().read_text().splitlines():
        m = pattern.match(line)
        if m:
            return m.group(1), m.group(2)
    raise SystemExit(f"no credentials line labelled {label!r} in {path}")


CLOCK_SERVER = ("import sys,time\n"
                "for _ in sys.stdin:\n"
                "    sys.stdout.write(str(time.time_ns())+chr(10)); sys.stdout.flush()\n")


def clock_offset(ssh_target, samples=40):
    """Host minus Mac wall clock: one persistent ssh channel, minimum-RTT midpoint."""
    proc = subprocess.Popen(
        ["ssh", "-o", "BatchMode=yes", ssh_target, "python3 -u -c " + shlex.quote(CLOCK_SERVER)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
    best = None
    try:
        for i in range(samples + 3):
            t0 = time.time()
            proc.stdin.write("\n")
            proc.stdin.flush()
            line = proc.stdout.readline()
            t1 = time.time()
            if not line.strip():
                break
            if i < 3:
                continue  # warm-up: channel and interpreter start
            rtt = t1 - t0
            if best is None or rtt < best[1]:
                best = (int(line) / 1e9 - (t0 + t1) / 2, rtt)
            time.sleep(0.03)
    except (OSError, ValueError) as error:
        log(f"clock offset failed: {error}")
    finally:
        proc.stdin.close()
        proc.terminate()
        proc.wait()
    if best is None:
        return None
    return {"offset_s": best[0], "rtt_s": best[1], "uncertainty_s": best[1] / 2,
            "method": "persistent ssh channel echo of time.time_ns(), min-RTT midpoint"}


class ScreenLockDir:
    """mkdir-based screen lock shared with other operators of this Mac."""

    def __init__(self, path, label):
        self.path, self.label, self.held = (pathlib.Path(path).expanduser() if path else None), label, False

    def __enter__(self):
        if not self.path:
            return self
        deadline = time.time() + 1800
        while True:
            try:
                self.path.mkdir()
                (self.path / "owner").write_text(f"realscreen {self.label} pid {os.getpid()} {time.ctime()}\n")
                self.held = True
                return self
            except FileExistsError:
                if time.time() > deadline:
                    raise SystemExit(f"screen lock {self.path} busy for 30 min")
                time.sleep(5)

    def __exit__(self, *_exc):
        if self.held:
            for child in self.path.iterdir():
                child.unlink()
            self.path.rmdir()


def cmd_run(args):
    with ScreenLockDir(args.lockdir, args.label):
        return run_locked(args)


def run_locked(args):
    out = pathlib.Path(args.out) / f"{time.strftime('%Y%m%dT%H%M%S')}-{args.label}"
    out.mkdir(parents=True, exist_ok=True)
    driver = build_driver()
    user, password = read_credentials(args.credentials, args.cred_label)
    host_user = args.host_user or user
    connect_host = args.connect_host or args.ssh.split("@")[-1]
    pin = (args.pin or pathlib.Path(args.pin_file).read_text()).strip().split("=")[-1]
    host_dir = args.host_dir
    run_dir = f"{host_dir}/runs/{out.name}"
    ssh(args.ssh, f"install -d -m 755 {host_dir} {host_dir}/runs", check=True)
    subprocess.run(["scp", "-q", *[str(HERE / f) for f in HOST_FILES], f"{args.ssh}:{host_dir}/"], check=True)
    ssh(args.ssh, f"chmod 755 {host_dir}/host_page.sh; chmod 644 {host_dir}/latency.html {host_dir}/page_server.py")

    deck_bin = pathlib.Path(args.deck_app) / "Contents" / "MacOS" / "arcen-deck"
    deck_args = [str(deck_bin), "--connect", connect_host, str(args.port), "--pin-sha256", pin,
                 "--pipeline", args.pipeline, "--tablet-mode", args.tablet_mode, "--credentials-stdin"]
    deck_args += shlex.split(args.deck_extra or "")
    meta = {
        "label": args.label, "ssh": args.ssh.split("@")[-1], "port": args.port, "pipeline": args.pipeline,
        "tablet_mode": args.tablet_mode, "device": args.device, "deck_app": args.deck_app,
        "deck_version": subprocess.run(["defaults", "read", str(pathlib.Path(args.deck_app) / "Contents" / "Info"),
                                        "CFBundleVersion"], capture_output=True, text=True).stdout.strip(),
        "pier_note": args.pier_note, "start_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "browser": args.browser, "url": args.url or "latency.html",
    }
    log(f"launching Deck {meta['deck_version']} → {connect_host}:{args.port} {args.pipeline}/{args.tablet_mode}")
    deck = subprocess.Popen(deck_args, stdin=subprocess.PIPE, stdout=open(out / "deck.out", "w"),
                            stderr=subprocess.STDOUT)
    deck.stdin.write(f"{user}\n{password}".encode())
    deck.stdin.close()
    password = None
    page = None
    try:
        query = "stress=0"
        page = ssh(args.ssh, f"{host_dir}/host_page.sh start {shlex.quote(host_user)} {run_dir} {args.page_port} {query}"
                             f" {shlex.quote(args.browser)} {shlex.quote(args.url or '')}",
                   timeout=180)
        meta["page_start"] = (page.stdout + page.stderr).strip()[-300:]
        log(f"host page: {meta['page_start']}")
        if page.returncode != 0:
            raise RuntimeError("host page did not start")
        meta["clock_before"] = clock_offset(args.ssh)
        log(f"clock offset {meta['clock_before']}")
        time.sleep(args.settle)
        drv = [str(driver), "--pid", str(deck.pid), "--out", str(out), "--device", args.device,
               "--rate", str(args.rate), "--phases", args.phases, "--draw-seconds", str(args.draw_seconds),
               "--hover-seconds", str(args.hover_seconds), "--stress-seconds", str(args.stress_seconds),
               "--jumps", str(args.jumps)]
        if args.escape_first:
            drv.append("--escape-first")
        if args.tilt:
            drv.append("--tilt")
        if args.region:
            drv += ["--region", args.region]
        if args.url:
            # A foreign page paints no code strip; snapshots are the evidence.
            drv += ["--strip-wait", "0", "--find-canvas"]
        log("driving: " + " ".join(drv[3:]))
        result = subprocess.run(drv, stderr=subprocess.PIPE, text=True, timeout=600)
        (out / "driver.log").write_text(result.stderr)
        log(result.stderr.strip().splitlines()[-1] if result.stderr.strip() else "driver silent")
        meta["clock_after"] = clock_offset(args.ssh)
        meta["host_chrony"] = ssh(args.ssh, "chronyc tracking 2>/dev/null | grep -E 'System time|Reference ID'").stdout.strip()
    finally:
        meta["end_utc"] = dt.datetime.now(dt.timezone.utc).isoformat()
        if page is not None:
            ssh(args.ssh, f"{host_dir}/host_page.sh stop {run_dir}")
            subprocess.run(["scp", "-q", f"{args.ssh}:{run_dir}/page.jsonl", f"{args.ssh}:{run_dir}/session.env",
                            f"{args.ssh}:{run_dir}/browser.txt", str(out)], capture_output=True)
            if (out / "browser.txt").exists():
                meta["browser_seen"] = (out / "browser.txt").read_text().strip()
        if deck.poll() is None:
            deck.send_signal(signal.SIGTERM)
            try:
                deck.wait(timeout=10)
            except subprocess.TimeoutExpired:
                deck.kill()
        if args.end_session:
            ssh(args.ssh, "for s in $(loginctl list-sessions --no-legend | awk '$3==\"%s\" {print $1}'); do "
                          "T=$(loginctl show-session $s -p Type --value); "
                          "case $T in x11|wayland) loginctl terminate-session $s;; esac; done" % host_user)
        if args.pier_log:
            since = meta["start_utc"][:19]
            # Pier logs are JSON lines with an ISO timestamp; keep this run's window.
            cmd = ("for f in %s; do [ -f \"$f\" ] && awk -v s=%s 'index($0,\"\\\"timestamp\\\":\\\"\")"
                   "{m=match($0,/\"timestamp\":\"[^\"]*\"/); ts=substr($0,RSTART+13,19); if (ts>=s) print}' \"$f\"; done"
                   % (args.pier_log, shlex.quote(since)))
            pier = ssh(args.ssh, cmd, timeout=120)
            (out / "pier.jsonl").write_text(pier.stdout)
        deck_log = sorted(pathlib.Path.home().glob("Library/Logs/Arcen/arcen-client.log.*"),
                          key=lambda p: p.stat().st_mtime)
        if deck_log:
            since = meta["start_utc"][:19]
            with open(out / "deck.jsonl", "w") as dst:
                for line in open(deck_log[-1], errors="replace"):
                    m = re.search(r'"timestamp":"([^"]{19})', line)
                    if m and m.group(1) >= since:
                        dst.write(line)
        (out / "run.json").write_text(json.dumps(meta, indent=2))
    if args.url:
        shots = sorted(p.name for p in out.glob("*.png"))
        (out / "summary.txt").write_text(f"== {args.label} {args.url} browser={meta.get('browser_seen', args.browser)}\n"
                                         f"snapshots: {', '.join(shots)}\n")
        print((out / "summary.txt").read_text())
        return meta
    report = analyze(out)
    print(json.dumps(report, indent=2))
    return report


# ---------------------------------------------------------------- analysis

def pct(values, q):
    if not values:
        return None
    s = sorted(values)
    return round(s[min(len(s) - 1, int(q * len(s)))], 1)


def stats(values):
    # Samples over 300 ms are link or host stalls; count them separately so a
    # single hiccup is visible without hiding the steady state.
    return {"n": len(values), "p50": pct(values, 0.5), "p95": pct(values, 0.95),
            "min": round(min(values), 1) if values else None,
            "over_300": sum(1 for v in values if v > 300)}


def fit(pairs):
    """Least-squares a*x+b for [(x, y)]."""
    n = len(pairs)
    if n < 2:
        return None
    mx = sum(p[0] for p in pairs) / n
    my = sum(p[1] for p in pairs) / n
    sxx = sum((p[0] - mx) ** 2 for p in pairs)
    if sxx == 0:
        return None
    a = sum((p[0] - mx) * (p[1] - my) for p in pairs) / sxx
    return a, my - a * mx


def load_jsonl(path):
    rows = []
    if path.exists():
        for line in open(path, errors="replace"):
            try:
                rows.append(json.loads(line))
            except ValueError:
                pass
    return rows


def analyze(out):
    out = pathlib.Path(out)
    meta = json.loads((out / "meta.json").read_text())
    run = json.loads((out / "run.json").read_text()) if (out / "run.json").exists() else {}
    sends = load_jsonl(out / "sends.jsonl")
    frames = load_jsonl(out / "frames.jsonl")
    batches = load_jsonl(out / "page.jsonl")
    win, cap = meta["window"], meta["capture"]
    sx, sy = win["w"] / cap["w"], win["h"] / cap["h"]
    anchor = meta["clock_anchor_start"]
    clocks = [c for c in (run.get("clock_before"), run.get("clock_after")) if c]
    clock = min(clocks, key=lambda c: c["rtt_s"]) if clocks else None
    offset = clock["offset_s"] if clock else 0.0
    if len(clocks) == 2:
        clock = dict(clock, drift_ms=round((clocks[1]["offset_s"] - clocks[0]["offset_s"]) * 1000, 2))

    def mono_to_unix(t):
        return anchor["unix"] + (t - anchor["mono"])

    def host_unix_to_mono(u):
        return u - offset - anchor["unix"] + anchor["mono"]

    phases = {}
    marks = [(list(m.keys())[0], list(m.values())[0]) for m in meta["phases"]]
    for i, (name, t) in enumerate(marks):
        if name != "end":
            phases[name] = (t, marks[i + 1][1] if i + 1 < len(marks) else t + 60)

    def phase_of(t):
        for name, (a, b) in phases.items():
            if a <= t < b:
                return name
        return None

    # Black/white blobs that never move are content (window controls, icons),
    # not a cursor: drop any position seen in most frames while the cursor moves.
    seen = {}
    moving = [f for f in frames if any(a <= f["t"] < b for n, (a, b) in phases.items() if n in ("hover", "draw"))]
    moving = moving or frames
    for f in moving:
        for b in f["blobs"]:
            key = (b["x0"] // 4, b["y0"] // 4, (b["x1"] - b["x0"]) // 4)
            seen[key] = seen.get(key, 0) + 1
    static = {k for k, n in seen.items() if n > 0.5 * max(1, len(moving))}
    for f in frames:
        f["blobs"] = [b for b in f["blobs"] if (b["x0"] // 4, b["y0"] // 4, (b["x1"] - b["x0"]) // 4) not in static]

    # Page events and frames on the Mac monotonic clock.
    events, pframes, info = [], [], {}
    for b in batches:
        origin = b["origin"]
        info = {k: b.get(k) for k in ("ua", "dpr", "screen", "inner")}
        for kind, ptype, x, y, pressure, buttons, ts, pnow, *extra in b["events"]:
            # Newer pages append tiltX, tiltY and the coalesced batch size.
            tx, ty, batch, pid = (list(extra) + [0, 0, 1, None][len(extra):])[:4]
            events.append({"kind": kind, "type": ptype, "x": x, "y": y, "p": pressure, "buttons": buttons,
                           "tx": tx, "ty": ty, "batch": batch, "pid": pid,
                           "t_ts": host_unix_to_mono((origin + ts) / 1000),
                           "t_handler": host_unix_to_mono((origin + pnow) / 1000)})
        for row in b["frames"]:
            # row[5] (performance.now() inside the callback) is when the frame was
            # really painted; row[1] is the refresh tick's start, which can precede
            # the input dispatched in that tick.
            painted = row[5] if len(row) > 5 else row[1]
            pframes.append({"n": row[0], "t": host_unix_to_mono((origin + painted) / 1000), "x": row[2], "y": row[3]})
    events.sort(key=lambda e: e["t_handler"])
    pframes.sort(key=lambda f: f["t"])
    moves = [s for s in sends if s["kind"] in ("move", "jump", "down")]

    # Mac point -> host CSS px. Start from the window geometry, refine on
    # calibration holds (each held send vs the last page sample before the next send).
    screen = info.get("screen") or [cap["w"], cap["h"]]
    gx = fit([(win["x"], 0), (win["x"] + win["w"], screen[0])])
    gy = fit([(win["y"], 0), (win["y"] + win["h"], screen[1])])
    held = [s for s in sends if s["phase"] in ("calib", "jump")]
    pairs_x, pairs_y = [], []
    for i, s in enumerate(held):
        end = held[i + 1]["t"] if i + 1 < len(held) else s["t"] + 0.45
        ex, ey = gx[0] * s["x"] + gx[1], gy[0] * s["y"] + gy[1]
        near = [e for e in events if e["kind"] == "move" and s["t"] - 0.05 < e["t_handler"] < end
                and abs(e["x"] - ex) < 40 and abs(e["y"] - ey) < 40]
        if near:
            pairs_x.append((s["x"], near[-1]["x"]))
            pairs_y.append((s["y"], near[-1]["y"]))

    def distinct(pairs):
        return len({round(p[0]) for p in pairs}) >= 3

    fx = (fit(pairs_x) if distinct(pairs_x) else None) or gx
    fy = (fit(pairs_y) if distinct(pairs_y) else None) or gy
    resid = [abs(fx[0] * a + fx[1] - b) for a, b in pairs_x] + [abs(fy[0] * a + fy[1] - b) for a, b in pairs_y]

    def to_host(x, y):
        return fx[0] * x + fx[1], fy[0] * y + fy[1]

    send_times = [s["t"] for s in moves]
    tol_host = max(2.5, 1.5 * abs(fx[0]))

    mapped = [to_host(s["x"], s["y"]) for s in moves]

    def match_send(hx, hy, t_hi, window=1.5, tol=None):
        """When did the sent path pass (hx, hy)? Latest pass at or before t_hi.

        Projects the point onto each segment between consecutive sends, so a
        host that interpolates or smooths positions (e.g. tablet smoothing)
        is timed by where on the path its sample lies. Returns a pseudo-send
        dict with the interpolated time, or None."""
        tol = tol or tol_host
        hi = bisect.bisect_right(send_times, t_hi)
        for i in range(hi - 1, -1, -1):
            s = moves[i]
            if t_hi - s["t"] > window:
                break
            ax, ay = mapped[i]
            if abs(ax - hx) + abs(ay - hy) <= tol:
                return {"t": s["t"], "phase": s["phase"]}
            if i == 0 or moves[i - 1]["phase"] != s["phase"]:
                continue
            bx, by = mapped[i - 1]
            dx, dy = ax - bx, ay - by
            seg = dx * dx + dy * dy
            if seg == 0:
                continue
            u = ((hx - bx) * dx + (hy - by) * dy) / seg
            if 0 <= u <= 1:
                px, py = bx + u * dx, by + u * dy
                if abs(px - hx) + abs(py - hy) <= tol:
                    t0 = moves[i - 1]["t"]
                    return {"t": t0 + u * (s["t"] - t0), "phase": s["phase"]}
        return None

    report = {"label": run.get("label") or out.name, "run": {k: run.get(k) for k in (
        "ssh", "port", "pipeline", "tablet_mode", "device", "deck_version", "pier_note", "start_utc")},
        "page": info, "clock": clock,
        "geometry": {"window_pt": [win["w"], win["h"]], "capture_px": [cap["w"], cap["h"]],
                     "refresh_hz": cap.get("refresh_hz"), "map_x": [round(v, 4) for v in fx],
                     "map_y": [round(v, 4) for v in fy], "calib_pairs": len(pairs_x),
                     "calib_resid_px_max": round(max(resid), 1) if resid else None},
        "frames": {"captured": len(frames), "decoded": sum(1 for f in frames if f["ok"])}}

    # Cursor blob (top-left) -> Mac point, calibrated on the holds.
    def blob_pt(b):
        return win["x"] + b["x0"] * sx, win["y"] + b["y0"] * sy

    cpx, cpy = [], []
    for i, s in enumerate(held):
        end = held[i + 1]["t"] if i + 1 < len(held) else s["t"] + 0.45
        late = [f for f in frames if end - 0.15 < f["t"] <= end and f["blobs"]]
        for f in late[-1:]:
            bx, by = min(((blob_pt(b), b) for b in f["blobs"]),
                         key=lambda v: abs(v[0][0] - s["x"]) + abs(v[0][1] - s["y"]))[0]
            if abs(bx - s["x"]) < 40 and abs(by - s["y"]) < 40:
                cpx.append((bx, s["x"]))
                cpy.append((by, s["y"]))
    cdx = sum(b - a for a, b in cpx) / len(cpx) if cpx else 0.0
    cdy = sum(b - a for a, b in cpy) / len(cpy) if cpy else 0.0

    def cursor_pt(b):
        x, y = blob_pt(b)
        return x + cdx, y + cdy

    # 1) Jumps: send -> host event -> page frame on glass, and local cursor on glass.
    jumps = [s for s in sends if s["kind"] == "jump"]
    j_host, j_ts, j_glass, j_cursor = [], [], [], []
    j_first, j_settle, j_steps = [], [], []
    for i, s in enumerate(jumps):
        until = jumps[i + 1]["t"] if i + 1 < len(jumps) else s["t"] + 0.5
        hx, hy = to_host(s["x"], s["y"])
        window_events = [e for e in events if s["t"] - 0.2 < e["t_handler"] < until + 0.3 and e["kind"] == "move"]
        ev = next((e for e in window_events if abs(e["x"] - hx) + abs(e["y"] - hy) <= tol_host), None)
        if ev:
            j_host.append((ev["t_handler"] - s["t"]) * 1000)
            j_ts.append((ev["t_ts"] - s["t"]) * 1000)
        if i > 0:
            # First sample that left the previous target: transport, before any smoothing.
            px, py = to_host(jumps[i - 1]["x"], jumps[i - 1]["y"])
            first = next((e for e in window_events if e["t_ts"] > s["t"] - 0.05
                          and abs(e["x"] - px) + abs(e["y"] - py) > 3 * tol_host), None)
            if first:
                j_first.append((first["t_ts"] - s["t"]) * 1000)
                if ev:
                    j_settle.append((ev["t_ts"] - first["t_ts"]) * 1000)
                    j_steps.append(sum(1 for e in window_events if first["t_ts"] <= e["t_ts"] <= ev["t_ts"]))
        fr = next((f for f in frames if s["t"] < f["t"] < until and f["ok"]
                   and abs(f["x"] - hx) + abs(f["y"] - hy) <= tol_host), None)
        if fr:
            j_glass.append((fr["t"] - s["t"]) * 1000)
        arrivals = []
        for f in frames:
            if not (s["t"] < f["t"] < until):
                continue
            for b in f["blobs"]:
                cx, cy = cursor_pt(b)
                if abs(cx - s["x"]) + abs(cy - s["y"]) <= 12:
                    arrivals.append(f["t"])
                    break
        if arrivals:
            j_cursor.append((arrivals[0] - s["t"]) * 1000)
    report["jump"] = {"input_to_host_handler_ms": stats(j_host), "input_to_host_event_timestamp_ms": stats(j_ts),
                      "input_to_host_first_motion_ms": stats(j_first), "host_first_motion_to_target_ms": stats(j_settle),
                      "host_samples_to_reach_target": stats(j_steps),
                      "motion_to_photon_page_ms": stats(j_glass),
                      "cursor_on_glass_ms": stats(j_cursor), "trials": len(jumps)}

    # 2) Continuous phases: per-sample host latency, per-frame glass latency,
    #    per-blob cursor latency, delivered fps on glass.
    for name in ("hover", "draw", "stress"):
        if name not in phases:
            continue
        a, b = phases[name]
        host_lat, ts_lat = [], []
        for e in events:
            if e["kind"] != "move" or not (a < e["t_handler"] < b + 0.5):
                continue
            s = match_send(e["x"], e["y"], e["t_handler"] + 0.02)
            if s and s["phase"] == name:
                host_lat.append((e["t_handler"] - s["t"]) * 1000)
                ts_lat.append((e["t_ts"] - s["t"]) * 1000)
        glass, prev = [], None
        host_render, render_to_glass = [], []
        page_by_counter = {}
        for pf in pframes:
            if a - 1 < pf["t"] < b + 1:
                page_by_counter.setdefault(pf["n"] & 0xFFFF, pf)
        cursor_lat, blobs2, prev_blobs = [], 0, set()
        ok_frames = [f for f in frames if a < f["t"] < b + 0.5]
        for f in ok_frames:
            if f["ok"] and (prev is None or (f["x"], f["y"]) != prev):
                prev = (f["x"], f["y"])
                s = match_send(f["x"], f["y"], f["t"])
                if s and s["phase"] == name:
                    glass.append((f["t"] - s["t"]) * 1000)
                    pf = page_by_counter.get(f["counter"])
                    if pf and pf["t"] >= s["t"] - 0.1:
                        host_render.append((pf["t"] - s["t"]) * 1000)
                        render_to_glass.append((f["t"] - pf["t"]) * 1000)
            if len(f["blobs"]) >= 2:
                blobs2 += 1
            positions = {(blob["x0"], blob["y0"]) for blob in f["blobs"]}
            fresh = [blob for blob in f["blobs"] if (blob["x0"], blob["y0"]) not in prev_blobs]
            prev_blobs = positions
            # Only a blob's first appearance at a position is an arrival.
            for blob in fresh:
                cx, cy = cursor_pt(blob)
                s = match_send(*to_host(cx, cy), f["t"], tol=max(tol_host, 6 * abs(fx[0])))
                if s and s["phase"] == name:
                    cursor_lat.append((f["t"] - s["t"]) * 1000)
        counters = [(f["t"], f["counter"]) for f in ok_frames if f["ok"]]
        changes, skipped, gaps, last = 0, 0, [], None
        for t, c in counters:
            if last is None:
                last = (t, c)
                continue
            if c != last[1]:
                delta = (c - last[1]) & 0xFFFF
                changes += 1
                skipped += max(0, delta - 1)
                gaps.append((t - last[0]) * 1000)
                last = (t, c)
        span = (counters[-1][0] - counters[0][0]) if len(counters) > 1 else 0
        page_n = [f for f in pframes if a < f["t"] < b]
        page_fps = (len(page_n) / (page_n[-1]["t"] - page_n[0]["t"])) if len(page_n) > 1 else None
        sends_n = sum(1 for s in sends if s["phase"] == name and s["kind"] == "move")
        events_n = sum(1 for e in events if e["kind"] == "move" and a < e["t_handler"] < b + 0.5)
        types = sorted({e["type"] for e in events if a < e["t_handler"] < b + 0.5})
        in_phase = [e for e in events if a < e["t_handler"] < b + 0.5]
        mv = [e for e in in_phase if e["kind"] == "move"]
        pressures = [e["p"] for e in mv]
        inked = [e for e in mv if e["buttons"] & 1]
        tilts = [(e["tx"], e["ty"]) for e in mv]
        steps = [((q["x"] - p["x"]) ** 2 + (q["y"] - p["y"]) ** 2) ** 0.5 for p, q in zip(inked, inked[1:])]
        dts = [(q["t_ts"] - p["t_ts"]) * 1000 for p, q in zip(mv, mv[1:])]
        stylus = {
            "pressure_levels": len({e["p"] for e in inked}),
            "tilt_x_range": [min(x for x, _ in tilts), max(x for x, _ in tilts)] if tilts else None,
            "tilt_y_range": [min(y for _, y in tilts), max(y for _, y in tilts)] if tilts else None,
            "coalesced_share": round(sum(1 for e in mv if e["batch"] > 1) / len(mv), 2) if mv else None,
            "coalesced_max_batch": max((e["batch"] for e in mv), default=None),
            "ink_samples": len(inked), "ink_max_step_px": round(max(steps), 1) if steps else None,
            "ink_step_px": stats(steps),
            "host_sample_interval_ms": stats(dts), "host_max_sample_gap_ms": round(max(dts), 1) if dts else None,
            "pointer_ids": sorted({str(e["pid"]) for e in in_phase}),
            "type_flips_in_ink": sum(1 for p, q in zip(inked, inked[1:]) if p["type"] != q["type"]),
            "event_types": {f"{k}:{t}": sum(1 for e in in_phase if (e["kind"], e["type"]) == (k, t))
                            for k, t in {(e["kind"], e["type"]) for e in in_phase}},
            "over_out": {f"{e['kind']}:{e['type']}": sum(1 for f in in_phase if (f["kind"], f["type"]) == (e["kind"], e["type"]))
                         for e in in_phase if e["kind"] in ("over", "out")},
        }
        report[name] = {
            "input_to_host_handler_ms": stats(host_lat), "input_to_host_event_timestamp_ms": stats(ts_lat),
            "motion_to_photon_page_ms": stats(glass), "input_to_page_raf_ms": stats(host_render),
            "page_raf_to_glass_ms": stats(render_to_glass), "cursor_on_glass_ms": stats(cursor_lat),
            "cursor_local_share": round(sum(1 for v in cursor_lat if v < 40) / len(cursor_lat), 2) if cursor_lat else None,
            "frames_with_two_cursor_blobs": blobs2,
            "glass_fps": round(changes / span, 1) if span else None, "page_fps": round(page_fps, 1) if page_fps else None,
            "page_frames_skipped_on_glass": skipped, "max_glass_gap_ms": round(max(gaps), 1) if gaps else None,
            "sends": sends_n, "host_move_events": events_n, "host_pointer_types": types,
            "host_pressure_range": [min(pressures), max(pressures)] if pressures else None,
            "stylus": stylus}

    report["deck_log"] = deck_summary(out / "deck.jsonl")
    report["pier_log"] = pier_summary(out / "pier.jsonl")
    (out / "report.json").write_text(json.dumps(report, indent=2))
    (out / "summary.txt").write_text(human(report))
    return report


def num(v):
    if isinstance(v, str) and v.startswith("Some("):
        v = v[5:-1]
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def deck_summary(path):
    rows = load_jsonl(path)
    if not rows:
        return None
    tel = [r.get("fields", {}) for r in rows if r.get("message") == "session telemetry"]
    hello = next((r.get("fields", {}) for r in rows if r.get("message") == "server hello"), {})
    keep = ("cursor", "tablet", "pipeline", "encoder", "width", "height", "fps", "login_window")
    out = {"server_hello": {k: v for k, v in hello.items() if any(w in k for w in keep)}}
    for key in ("wire_delay_ms", "wire_clock_age_ms", "rx_fps", "host_fps", "presented_fps_confirmed", "decode_ms",
                "presenter_drops", "presenter_skips", "ui_passes_per_s", "deck_cpu_percent"):
        vals = [x for x in (num(f.get(key)) for f in tel) if x is not None]
        if vals:
            out[key] = stats(vals)
    census = {}
    for r in rows:
        m = r.get("message", "")
        if any(w in m.lower() for w in ("cursor", "tablet", "pen", "degrad", "pipeline", "plan")):
            census[m[:90]] = census.get(m[:90], 0) + 1
    out["census"] = dict(sorted(census.items(), key=lambda kv: -kv[1])[:25])
    return out


def pier_summary(path):
    rows = load_jsonl(path)
    if not rows:
        return None

    def message(r):
        return (r.get("fields") or {}).get("message") or r.get("message") or ""

    census, last = {}, {}
    for r in rows:
        m = message(r)
        census[m[:90]] = census.get(m[:90], 0) + 1
        low = m.lower()
        if any(w in low for w in ("plan", "cursor", "tablet", "pen", "stage", "streaming", "degrad", "ready",
                                  "capture", "encoder", "why", "fps")):
            fields = {k: v for k, v in (r.get("fields") or {}).items() if k != "message"}
            last[m[:90]] = json.dumps(fields)[:600]
    key = {}
    enc_ms, enc_fps, rtts, qdelay = [], [], [], []
    for r in rows:
        m = message(r)
        f = r.get("fields") or {}
        if m.startswith("[capenc] enc_fps="):
            kv = dict(re.findall(r"(\w+)=([0-9.]+)", m))
            if "avg_encode_ms" in kv:
                enc_ms.append(float(kv["avg_encode_ms"]))
            enc_fps.append(float(kv["enc_fps"]))
        elif m == "encoder bitrate follows QUIC path":
            for name, bucket in (("rtt_ms", rtts), ("queue_delay_ms", qdelay)):
                v = num(f.get(name))
                if v is not None:
                    bucket.append(v)
        elif m == "capenc ready":
            key["capenc"] = {k: f.get(k) for k in ("backend", "codec", "chroma", "cursor", "fps", "width", "height")}
        elif m == "tablet mode result":
            key["tablet"] = {k: f.get(k) for k in ("requested", "active", "pen_unavailable", "reason")}
        elif m == "media plan active":
            key["plan"] = {k: f.get(k) for k in ("requested_codec", "resolved_codec", "resolved_fps", "resolved_width",
                                                  "resolved_height", "resolved_encoder_backend")}
        elif "desktop for this sign-in is created at this depth" in m:
            key["desktop_depth"] = f.get("depth")
        elif "cursor" in m.lower() and ("authority" in m.lower() or "mode" in m.lower()):
            key.setdefault("cursor_lines", {})[m[:80]] = json.dumps(f)[:200]
    key["capenc_encode_ms"] = stats(enc_ms) if enc_ms else None
    key["capenc_fps"] = stats(enc_fps) if enc_fps else None
    key["quic_rtt_ms"] = stats(rtts) if rtts else None
    key["quic_queue_delay_ms"] = stats(qdelay) if qdelay else None
    return {"lines": len(rows), "key": key,
            "census_top": dict(sorted(census.items(), key=lambda kv: -kv[1])[:30]),
            "last_key_lines": last}


def human(r):
    lines = [f"== {r['label']}  {r['run'].get('pipeline')}/{r['run'].get('tablet_mode')} device={r['run'].get('device')}"
             f" deck={r['run'].get('deck_version')} pier={r['run'].get('pier_note')}"]
    c = r.get("clock") or {}
    lines.append(f"clock offset host-mac {c.get('offset_s', 0) * 1000:.1f} ms ± {c.get('uncertainty_s', 0) * 1000:.1f} ms"
                 f" ({c.get('method', 'none')}); capture {r['geometry']['capture_px']} @{r['geometry']['refresh_hz']} Hz;"
                 f" frames {r['frames']['decoded']}/{r['frames']['captured']} decoded; calib pairs {r['geometry']['calib_pairs']}")

    def s(d):
        if not d or not d["n"]:
            return "n/a"
        stall = f", {d['over_300']}>300ms" if d.get("over_300") else ""
        return f"p50 {d['p50']} p95 {d['p95']} (n={d['n']}{stall})"

    j = r.get("jump", {})
    lines.append(f"jump: cursor on glass {s(j.get('cursor_on_glass_ms'))} | input→host handler {s(j.get('input_to_host_handler_ms'))}"
                 f" | input→host event.timeStamp {s(j.get('input_to_host_event_timestamp_ms'))}"
                 f" | first host motion {s(j.get('input_to_host_first_motion_ms'))}"
                 f" | first motion→at target {s(j.get('host_first_motion_to_target_ms'))}"
                 f" samples {s(j.get('host_samples_to_reach_target'))}"
                 f" | motion-to-photon {s(j.get('motion_to_photon_page_ms'))}")
    for name in ("hover", "draw", "stress"):
        p = r.get(name)
        if not p:
            continue
        lines.append(f"{name}: cursor {s(p['cursor_on_glass_ms'])} local-share {p['cursor_local_share']} | input→host handler {s(p['input_to_host_handler_ms'])}"
                     f" | input→host event.timeStamp {s(p['input_to_host_event_timestamp_ms'])}"
                     f" | input→page rAF {s(p['input_to_page_raf_ms'])} | rAF→glass {s(p['page_raf_to_glass_ms'])}"
                     f" | motion-to-photon {s(p['motion_to_photon_page_ms'])} | glass {p['glass_fps']} fps (page {p['page_fps']}),"
                     f" skipped {p['page_frames_skipped_on_glass']}, max gap {p['max_glass_gap_ms']} ms | types {p['host_pointer_types']}"
                     f" pressure {p['host_pressure_range']}")
        st = p.get("stylus")
        if st:
            lines.append(f"{name} stylus: pressure levels {st['pressure_levels']} tiltX {st['tilt_x_range']} tiltY {st['tilt_y_range']}"
                         f" | coalesced share {st['coalesced_share']} max batch {st['coalesced_max_batch']}"
                         f" | ink samples {st['ink_samples']} step px {s(st['ink_step_px'])} max {st['ink_max_step_px']}"
                         f" | host sample interval {s(st['host_sample_interval_ms'])} max gap {st['host_max_sample_gap_ms']} ms"
                         f" | over/out {st['over_out']} | ids {st['pointer_ids']} type flips in ink {st['type_flips_in_ink']}"
                         f" | events {st['event_types']}")
    return "\n".join(lines) + "\n"


def table(dirs):
    """Markdown table, one row per run: p50 (p95) in ms."""
    head = ("| run | Deck | Pier | pipeline | device | cursor on glass | input→host evt (draw) | input→page rAF (draw)"
            " | rAF→glass (draw) | motion-to-photon (draw) | jump m2p | glass fps / page fps (draw) | stalls>300ms |")
    rows = [head, "|" + "---|" * head.count("|")[:-1] if False else "|" + "---|" * (head.count("|") - 1)]
    for d in dirs:
        path = pathlib.Path(d) / "report.json"
        if not path.exists():
            continue
        r = json.loads(path.read_text())
        dr, j = r.get("draw") or {}, r.get("jump") or {}

        def c(x):
            return f"{x['p50']} ({x['p95']})" if x and x.get("n") else "n/a"

        stalls = sum((v or {}).get("over_300", 0) for p in ("hover", "draw", "stress")
                     for k, v in (r.get(p) or {}).items() if isinstance(v, dict) and k == "input_to_host_event_timestamp_ms")
        rows.append(f"| {r['label']} | {r['run'].get('deck_version')} | {r['run'].get('pier_note')} | {r['run'].get('pipeline')}"
                    f" | {r['run'].get('device')} | {c(dr.get('cursor_on_glass_ms'))} | {c(dr.get('input_to_host_event_timestamp_ms'))}"
                    f" | {c(dr.get('input_to_page_raf_ms'))} | {c(dr.get('page_raf_to_glass_ms'))}"
                    f" | {c(dr.get('motion_to_photon_page_ms'))} | {c(j.get('motion_to_photon_page_ms'))}"
                    f" | {dr.get('glass_fps')} / {dr.get('page_fps')} | {stalls} |")
    return "\n".join(rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run")
    run.add_argument("--ssh", required=True, help="root@host used to place the page and read logs")
    run.add_argument("--connect-host", help="address the Deck connects to (default: the ssh host)")
    run.add_argument("--port", type=int, default=18444)
    run.add_argument("--pin", help="certificate SHA-256 (colon hex)")
    run.add_argument("--pin-file")
    run.add_argument("--deck-app", required=True)
    run.add_argument("--deck-extra", help="extra Deck arguments (shell-quoted)")
    run.add_argument("--pipeline", default="auto", choices=("auto", "speed", "grading", "hdr"))
    run.add_argument("--tablet-mode", default="light", choices=("light", "hard", "off"))
    run.add_argument("--device", default="mouse", choices=("mouse", "pen"))
    run.add_argument("--rate", type=float, default=120)
    run.add_argument("--phases", default="focus,calib,jump,hover,draw,stress")
    run.add_argument("--jumps", type=int, default=16)
    run.add_argument("--hover-seconds", type=float, default=5)
    run.add_argument("--draw-seconds", type=float, default=8)
    run.add_argument("--stress-seconds", type=float, default=8)
    run.add_argument("--browser", default="firefox",
                     choices=("firefox", "firefox-x11", "chrome", "chrome-wayland", "chrome-x11"),
                     help="host browser: firefox (Wayland-native in a Wayland session), firefox-x11 (Xwayland),"
                          " chrome (its own default platform), chrome-wayland, chrome-x11")
    run.add_argument("--url", help="open this page instead of latency.html (no latency analysis; snapshots only)")
    run.add_argument("--tilt", action="store_true", help="pen: vary tilt during strokes")
    run.add_argument("--region", help="stylus phase: canvas box as window fractions x0,y0,x1,y1")
    run.add_argument("--settle", type=float, default=3, help="seconds after the page is up before driving")
    run.add_argument("--escape-first", action="store_true", help="press Escape first (GNOME overview)")
    run.add_argument("--credentials", default="~/.arcen-nightly.env")
    run.add_argument("--cred-label", default="Shared AD user")
    run.add_argument("--host-user", help="host account owning the desktop (default: credential user)")
    run.add_argument("--host-dir", default="/opt/arcen-build/realscreen")
    run.add_argument("--page-port", type=int, default=18601)
    run.add_argument("--pier-log", help="host path glob(s) of Pier JSON logs to slice into the run")
    run.add_argument("--pier-note", default="", help="free text: Pier build/SHA/desktop depth")
    run.add_argument("--end-session", action="store_true", help="terminate the host desktop session afterwards")
    run.add_argument("--lockdir", help="mkdir-style screen lock to hold for the run (waits while it exists)")
    run.add_argument("--label", required=True)
    run.add_argument("--out", default="realscreen-results")
    an = sub.add_parser("analyze")
    an.add_argument("dir")
    tb = sub.add_parser("table", help="markdown table over finished runs")
    tb.add_argument("dirs", nargs="+")
    args = ap.parse_args()
    if args.cmd == "run":
        if not (args.pin or args.pin_file):
            ap.error("--pin or --pin-file is required")
        cmd_run(args)
    elif args.cmd == "table":
        print(table(args.dirs))
    else:
        report = analyze(args.dir)
        print(human(report))


if __name__ == "__main__":
    main()
