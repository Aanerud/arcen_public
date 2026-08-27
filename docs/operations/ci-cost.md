# CI Cost and Actions-Minute Triage

Audience: Arcen maintainers who are about to iterate through GitHub Actions.
This note records the 2026-08-04 CI repair incident so the next person can see
what each push costs before the monthly Actions allowance is already gone.

On 2026-08-04, repairing CI consumed roughly **520 billable Actions minutes in a
single afternoon**, moving the account to about 92% of its monthly allowance.
The trap was not one slow job. It was repeatedly paying for every platform while
fixing failures that could not be reproduced on a macOS workstation.

## Why a short macOS job dominates the bill

GitHub bills hosted runners by OS multiplier, not just wall-clock time:

| Runner | Billing multiplier |
| --- | --- |
| Linux | 1x |
| Windows | 2x |
| macOS | 10x |

A full Arcen CI round is therefore roughly this expensive:

| Job | Wall clock | Multiplier | Billed |
| --- | --- | --- | --- |
| Arcen Deck (macOS client) | ~4 min | 10x | ~40 min |
| Arcen Pier (Windows host) | ~13 min | 2x | ~26 min |
| Arcen Pier (Linux host) | ~5 min | 1x | ~5 min |
| Shared crates and issuer | ~3 min | 1x | ~3 min |
| Documentation guard + Gitleaks | seconds | 1x | ~2 min |

The four-minute macOS Deck job is about **55% of the bill** for a full round.
Before path filtering, even a docs-only PR could pay roughly 76 billable minutes
because it still ran the macOS and Windows jobs.

## What the workflow runs now

`.github/workflows/ci.yml` has a `changes` job that compares the current commit
with the pull-request base SHA, or with `github.event.before` on a push. It emits
four booleans: `shared`, `deck`, `linux`, and `windows`. The delivery jobs run
only when their boolean is `true`. The documentation guard and Gitleaks secret
scan still run independently on every workflow run.

The filter deliberately fails safe. These cases force the full matrix:

- `workflow_dispatch` manual runs;
- no usable base commit;
- an empty diff, because the workflow does not trust it;
- changes to `Cargo.toml`, `Cargo.lock`, `rust-toolchain`, or anything under
  `.github/workflows/`;
- changes under `shared/`, `scripts/`, or `tools/`.

That means "I only edited the workflow file" is still a full macOS + Windows +
Linux + shared run. Treat workflow edits as expensive.

If none of those fail-safe rules match, the workflow selects jobs by path:

| Changed path | Jobs selected |
| --- | --- |
| `clients/macos/`, `packaging/macos/` | Arcen Deck (macOS client) |
| `hosts/(linux|capenc|audiocap|input-helper)/`, `packaging/linux/` | Arcen Pier (Linux host) |
| `hosts/windows/`, `packaging/windows/` | Arcen Pier (Windows host) |
| `hosts/windows/cp-ipc/` | Shared crates and issuer, and also Windows host because it is under `hosts/windows/` |
| Docs-only paths outside the fail-safe list | Documentation guard + Gitleaks only |

Read the workflow before relying on this table if it has changed; the table is a
summary of `ci.yml` at commit `7d15761`.

## Check the cost of a run yourself

Use the run's job timestamps and apply the runner multiplier yourself:

```sh
gh run view <run-id> --repo Aanerud/arcen_public --json jobs
```

Each job includes step `startedAt` and `completedAt` fields. Compute wall-clock
minutes for the job, then multiply by the runner OS: Linux 1x, Windows 2x,
macOS 10x. The GitHub API timing endpoint is not reliable for this repository:

```sh
gh api /repos/Aanerud/arcen_public/actions/runs/<run-id>/timing
```

It returns zeros here, so do not use it for cost accounting.

Before a long iteration loop, check the remaining allowance in GitHub's billing
UI. Read-only `gh` queries do not spend Actions minutes; pushes and workflow
reruns do.

## Avoid burning the allowance

Verify locally before pushing. The root README and workflow list the supported
local checks; for shared crates that includes formatting, strict Clippy, tests,
and the dependency-graph guard commands. Local macOS can cover the Deck and
shared crates well, but it cannot reproduce Linux Pier or Windows Pier builds.

Use `[skip ci]` in a commit message only when the change genuinely does not need
CI, such as a text-only correction that does not affect guarded content. Do not
use it for workflow, lockfile, toolchain, shared crate, packaging, host, client,
security, licensing, or release-behavior changes. Skipping CI saves minutes only
when the risk is honestly lower than the cost.

One trap worth knowing: GitHub scans the **whole** commit message for the skip
directive, body included. A commit that merely *mentions* it in prose — for
example a message explaining why an earlier commit used one — will itself be
skipped, silently, with no run appearing anywhere. This document was committed
that way on its first attempt and produced no CI run at all. If a push seems to
trigger nothing, check the commit body before looking anywhere else.

Cancel superseded runs once a newer push makes them irrelevant:

```sh
gh run cancel <run-id> --repo Aanerud/arcen_public
```

The workflow also has per-ref concurrency with `cancel-in-progress: true`, but
cancel explicitly when you see an obsolete expensive run still burning minutes.

## Measured effect

The first documentation-only pull request under path filtering (#131) ran the
`changes` job, the documentation guard and the secret scan, skipped all four
delivery jobs, and cost **3 billable minutes instead of 76**. The `changes` job
log shows the diff it saw and the booleans it computed, which is the way to
confirm the filter is genuinely deciding rather than defaulting to skip
everything -- a filter stuck at "false" would look exactly like a green build.

Two further savings landed unverified, because editing the workflow forces a
full matrix and the allowance was nearly gone:

- The Windows installer was being built twice; `build.cmd` already builds and
  WSS-verifies it, so CI now only runs its tests.

## The Windows build is nine minutes and that is not a cache bug

Worth writing down, because it looks exactly like one and was misdiagnosed once
already.

The MSVC step recompiles about 175 crates on every run **despite a cache hit**
that restores 687 MB. The obvious explanation is that `hosts\windows\build.cmd`
redirects `CARGO_TARGET_DIR` to `target\windows-package`, which the default
`. -> target` cache workspace would miss. That explanation is wrong, and the
workflow log says so directly: rust-cache prints its cached path as
`D:\a\arcen\arcen\target`, the whole tree, so the nested directory was always
included. Adding `cache-workspaces: . -> target/windows-package` only duplicates
it inside the archive and makes every run slower.

The real cause is one line, `hosts\windows\build.cmd:42`:

```bat
if exist "%PACKAGE_TARGET%" rmdir /s /q "%PACKAGE_TARGET%"
```

The build deletes its target directory first, so a shipped binary cannot contain
a stale artifact. That is a deliberate release-integrity guarantee and the nine
minutes is its price. Changing it trades a reproducible release build for CI
minutes, which is Release/Security's call, not a workflow tweak.

If you want to check this yourself rather than take it on trust, look for
`Cache Paths:` in the `setup-rust-toolchain` step of any Windows run.

Confirm both on the first real pull request that touches the Windows host.

## When local verification cannot cover the platform

The lab hosts exist so platform work does not have to be iterated through CI:

- Linux Pier: `root@<your-pier-host>`
- Windows Pier: `admin@<your-pier-host>`

Use the existing driver from the workstation:

```sh
packaging/build-lab-installers.sh              # both platforms
packaging/build-lab-installers.sh --linux      # Linux only
packaging/build-lab-installers.sh --windows    # Windows only
```

Set `LINUX_HOST` or `WINDOWS_HOST` if you need a non-default target. The script
builds lab installers, so its artifacts are for
platform verification only and must not be published as releases.

The 2026-08-04 failure mode was seven CI rounds because the lab hosts were not
available and each push paid for all six jobs. If a platform cannot be verified
locally and the lab is down, stop and estimate the Actions cost before starting a
push-and-watch loop.
