#!/usr/bin/env bash
# Copy the working tree to a build host that has no git checkout.
#
# The Windows build host builds from a copied source tree rather than a clone.
# Doing that by hand is how private material escapes: this repository keeps lab
# inventories in .claude/, a third-party reference corpus in refference/, key
# material in keys/, and built artefacts in dist/. None of it may be copied to
# another machine, and none of it may end up in a build directory that someone
# later archives. The exclusions belong in a script, not in whoever is typing.
#
# It also disables AppleDouble metadata. macOS bsdtar writes a `._name`
# sidecar for every file carrying extended attributes, and extracting those on
# Windows leaves stray `._ARCEN_NOTICE.md` style files that
# scripts/verify-opusic-source.ps1 correctly rejects as unreviewed governed
# source. The build then fails for a reason that has nothing to do with the
# code. COPYFILE_DISABLE=1 suppresses them at the source.
#
# Usage: scripts/sync-to-build-host.sh <user@host> <remote-path>
#   e.g. scripts/sync-to-build-host.sh admin@pier.example.internal 'C:/arcen-main'
#        scripts/sync-to-build-host.sh root@pier.example.internal /root/arcen-main
#
# The remote shell is detected rather than assumed. The Windows hosts answer
# SSH with PowerShell and the Linux hosts with a POSIX shell, and a PowerShell
# command block sent to bash fails with a syntax error that looks nothing like
# the actual problem.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <user@host> <remote-path>" >&2
    exit 2
fi

destination=$1
remote_path=$2
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"

# Never copied. `.git` and `target` are merely large; the rest are private or
# are build output that must be produced on the target, not carried to it.
excludes=(
    --exclude=./.git
    --exclude=./target
    --exclude=./refference
    --exclude=./keys
    --exclude=./dist
    --exclude=./dist-installers
    --exclude=./.claude
    --exclude=./.local
    --exclude="./Arcen Deck.app"
    --exclude="./*.private.md"
)

echo "==> syncing $repo -> $destination:$remote_path"
echo "    excluded: .git target refference keys dist dist-installers .claude .local 'Arcen Deck.app'"

# Detect the remote shell instead of assuming PowerShell. `uname -s` is a
# builtin-free POSIX probe; PowerShell has no `uname`, so a non-zero exit or
# empty answer means Windows.
if remote_kind=$(ssh -o BatchMode=yes "$destination" 'uname -s' 2>/dev/null) &&
    [ -n "${remote_kind//[$'\r\n ']/}" ]; then
    echo "    remote shell: POSIX ($(echo "$remote_kind" | tr -d '\r\n'))"
    extract_cmd="rm -rf '$remote_path' && mkdir -p '$remote_path' && tar -xzf - -C '$remote_path'"
    stray_cmd="find '$remote_path' -name '._*' -type f 2>/dev/null | wc -l"
else
    echo "    remote shell: PowerShell (Windows)"
    extract_cmd="if (Test-Path '$remote_path') { Remove-Item -LiteralPath '$remote_path' -Recurse -Force }; \
         New-Item -ItemType Directory -Force -Path '$remote_path' | Out-Null; \
         tar -xzf - -C '$remote_path'"
    stray_cmd="Get-ChildItem -LiteralPath '$remote_path' -Recurse -Force -Filter '._*' -ErrorAction SilentlyContinue | Measure-Object | Select-Object -ExpandProperty Count"
fi

# Refuse to run if the private paths would somehow be included, rather than
# trusting the exclude list to be correct.
staged=$(COPYFILE_DISABLE=1 tar -cz "${excludes[@]}" -C "$repo" . | tar -tz)
if printf '%s\n' "$staged" | grep -qE '^\./(refference|keys|\.claude|\.local|dist)/'; then
    echo "error: private paths present in the archive; refusing to transfer" >&2
    printf '%s\n' "$staged" | grep -E '^\./(refference|keys|\.claude|\.local|dist)/' | head >&2
    exit 1
fi

COPYFILE_DISABLE=1 tar -cz "${excludes[@]}" -C "$repo" . |
    ssh -o BatchMode=yes "$destination" "$extract_cmd"

# AppleDouble suppression is the point of COPYFILE_DISABLE, so verify it worked
# rather than assuming.
strays=$(ssh -o BatchMode=yes "$destination" "$stray_cmd")
if [ "${strays//[$'\r\n ']/}" != "0" ]; then
    echo "error: $strays AppleDouble ._* files reached the build host" >&2
    exit 1
fi

echo "==> synced clean; no AppleDouble sidecars present"
