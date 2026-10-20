# Real-screen latency harness

Measures what a person at the Deck actually sees: the real Arcen Deck GUI in a
visible window on a Mac, connected to a real Pier, driven with real macOS input
events, while a real browser in the host desktop records what arrived and
paints what the Deck then shows. Use it for anything about feel or latency;
headless smokes (`media-smoke`, `input-smoke`) do not measure feel.

Files live in [`scripts/lab/realscreen/`](../../scripts/lab/realscreen/):

| File | Runs on | Does |
| --- | --- | --- |
| `latency.html` | host browser | Logs every pointer sample (coalesced too) with `event.timeStamp`, `performance.now()`, pointer type and id, pressure, tilt and coalesced-batch size, plus pointerover/out/cancel; draws green ink at once; paints a code strip (top left) each animation frame carrying the frame counter and the last pointer position; `S` toggles a moving stress band. |
| `page_server.py` | host | Serves the page on `127.0.0.1` and appends the page's log batches to `page.jsonl`. |
| `host_page.sh` | host (root) | Finds the user's graphical session (X11 or Wayland), starts the server as that user and opens the page (or `--url`) in a kiosk browser with a fresh profile (`firefox`, `firefox-x11`, `chrome`, `chrome-wayland`, `chrome-x11`), recording the browser version and the display connection it made in `browser.txt`; `stop` ends both by PID. |
| `driver.swift` | Mac | Finds the Deck window, captures it with ScreenCaptureKit (cursor included) at up to the display rate, decodes the strip and finds black/white cursor blobs in every frame, and posts `CGEvent` mouse or synthetic tablet (pen, with pressure, proximity and, with `--tilt`, tilt) input. |
| `realscreen.py` | Mac | Orchestrates one run end to end and analyses it (`run`, `analyze`). |

## What it reports

All times are milliseconds, p50/p95 over the phase.

- **cursor on glass** — input posted → the cursor seen at the new position in
  the captured Deck window. A Deck-drawn (local) cursor lands in about one or
  two display refreshes; a cursor that comes back in the video takes as long
  as the video. `cursor_local_share` is the fraction of arrivals under 40 ms.
- **input → host event.timeStamp** — input posted → the browser's event
  timestamp on the host. This is transport plus host input injection. On
  X11 Firefox stamps events with the X server time (whole milliseconds).
- **input → host handler** — to the page's handler running. Includes the time
  the browser holds coalesced moves until its next refresh tick, so a slow
  host browser shows here.
- **input → page rAF** — to the page painting the frame that first carries the
  position (`performance.now()` inside the animation-frame callback).
- **rAF → glass** — that page frame → the Deck window showing it: browser
  composition, capture, encode, network, decode and presentation.
- **motion-to-photon** — input posted → the Deck window showing a page frame
  that carries that input: the whole chain.
- **glass fps / page fps / skipped / max gap** — distinct page frames reaching
  the Deck window per second, the page's own frame rate, page frames that
  never reached the glass, and the longest freeze.
- **host pointer types / pressure** — what the host browser saw (`mouse` or
  `pen`), proving whether pen input arrived as a pen.
- **stylus** (per phase) — distinct pressure levels in ink, tilt ranges,
  coalesced share and largest batch, ink step and host sample interval, the
  pointer ids seen, pointer-type flips inside ink, and an event census
  (`down:mouse` in a pen stroke means the pen reached the page as a mouse).
- **jump: first host motion / first motion → at target** — for the step
  phase: when the first host sample left the previous point, and how long
  and how many samples it took to reach the target. A host that smooths
  positions arrives on time but needs several samples to settle.
- **`over_300`** — samples later than 300 ms, reported apart from the
  percentiles: these are link or host stalls (the Pier log's QUIC `rtt_ms`
  and `queue_delay_ms` say which).

Positions are matched along the sent path (each host sample is timed by
where it lies between consecutive sends), so a host that interpolates or
smooths positions — tablet smoothing, for example — shows that lag as
latency instead of failing to match.

The phases are `focus` (click into the page), `calib` (nine held points; fits
the Mac-point → host-pixel map), `jump` (alternating 0.5 s holds: clean step
response), `hover` and `draw` (a circle at `--rate`, 120 Hz by default;
`draw` holds the button or pen tip, with varying pressure for the pen), and
`stress` (the moving band on, for frame rate under load).

### Clocks

Mac times are `mach_absolute_time` (`CACurrentMediaTime`, ScreenCaptureKit
`displayTime`). Host times are the browser's clock mapped through the host's
`CLOCK_REALTIME`. The offset is measured before and after each run over one
persistent ssh channel (minimum round-trip midpoint); the report states the
offset, its ± bound (half the minimum RTT) and `chronyc tracking`. Numbers
that cross machines (input → host, rAF → glass) carry that uncertainty; the
Deck-only numbers (cursor on glass, motion-to-photon) do not.

When the Deck connects over a long or VPN link, label Deck-side numbers with
that link. They describe that link, not the host; never turn them into
product caps or thresholds.

## Requirements

- macOS with Xcode command-line tools (`swiftc`), Python 3, and the
  terminal that runs the harness granted **Accessibility** (to post events)
  and **Screen Recording** (to capture). `realscreen-driver --probe` prints
  the three preflight results.
- The Deck `.app` to test. `--connect` quick-connect is used, so no UI
  driving is needed to log in; credentials go to the Deck on stdin.
- On the host: root ssh, `python3`, `firefox`, and a Pier whose session will
  belong to the account you log in with. A dev Pier started from an ssh
  shell must run outside that shell's logind session (for example under
  `systemd-run`), or the session launcher fails with "pam_systemd did not
  create the required logind session".
- A host browser that renders in software (an Xorg desktop at depth 30 can
  do that to Firefox) paints a few frames a second and dominates every
  number; check `page fps` before reading anything else.
- A local credentials file (default `~/.arcen-nightly.env`, mode 0600)
  containing a line `<label> : <user> / <password>`; `--cred-label` picks the
  line. Never commit it.

## Running

```sh
python3 scripts/lab/realscreen/realscreen.py run \
  --ssh root@pier.example.internal --port 18444 \
  --pin-file host.cert-sha256 \
  --deck-app "/Applications/Arcen Deck.app" \
  --pipeline auto --tablet-mode light --device mouse \
  --pier-log "/var/log/arcen/arcen-pier.log" --pier-note "0.17.0 depth 24" \
  --label x11-auto-mouse --out ~/realscreen-results
```

- `--device pen` posts synthetic tablet events (proximity in/out, tablet-point
  subtype, pressure) for the jump, hover and draw phases; `--tablet-mode` is
  what the Deck requests.
- `--phases` selects phases; `--escape-first` presses Escape before the click
  (a fresh GNOME login opens the overview).
- `--browser` picks the host browser; `--tilt` varies pen tilt.
- `--url URL --phases stylus` opens a real page instead (for example a stylus
  test site) and draws on its largest near-black box (its canvas): it clicks
  the page margin for keyboard focus, hovers the canvas, taps once (pages
  change layout on the first contact), scrolls with arrow keys until the
  canvas is in view, then draws a hover pass, a pressure ramp, a tilted stroke
  and a fast wavy stroke, with snapshots mid-stroke. There is no latency
  analysis for a foreign page; the snapshots are the evidence.
- `--end-session` terminates the user's graphical session afterwards. Use it
  when the next run targets a different Pier on the same host: one X11
  desktop per host.
- The run directory holds `summary.txt`, `report.json`, the raw
  `sends.jsonl`/`frames.jsonl`/`page.jsonl`, the Deck and Pier log slices,
  and PNG snapshots. `realscreen.py analyze <dir>` re-runs the analysis.

Only one run may drive the Mac's screen at a time. `--lockdir <dir>` holds
a `mkdir` lock for the whole run (waiting while the directory exists, with an
`owner` file inside) — use the same directory as anyone else driving that
Mac. The driver also waits on `/tmp/arcen-mac-screen.lock` (`flock`) when
that file exists; `--create-lock` on the driver creates it.

`realscreen.py table <run dirs>` prints a Markdown table (p50 with p95 in
brackets) across finished runs.

## Limits

- The capture shows what the window server composes, which is a refresh or
  so before the panel lights; treat cursor-on-glass as relative.
- The cursor finder looks for small black-and-white blobs on the page's grey
  background; a cursor theme without both colours or content with them
  (notifications) can confuse it. Static blobs are ignored.
- Synthetic tablet events exercise the Deck's AppKit tablet path; they come
  from no real HID tablet, so Hard (USB) tablet mode is not covered, and they
  carry no barrel buttons. Their tablet fields (tilt included) must be set
  after the tablet subtype, which clears them.
