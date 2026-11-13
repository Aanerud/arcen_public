#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# Copy the working tree to a build host that has no git checkout.
#
# Only the source allowlist is copied: files git tracks, plus new files git
# does not ignore (`git ls-files -co --exclude-standard`). Everything ignored —
# build output, local key material, built artefacts and any other excluded
# local or private material — stays on this machine, so nothing outside the
# source tree can end up in a build directory that someone later archives.
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

# The source allowlist, NUL-separated so names with spaces survive.
file_list=$(mktemp "${TMPDIR:-/tmp}/arcen-sync.XXXXXX")
trap 'rm -f "$file_list"' EXIT
git -C "$repo" ls-files -z -co --exclude-standard >"$file_list"

echo "==> syncing $repo -> $destination:$remote_path"
echo "    copying $(tr -cd '\0' <"$file_list" | wc -c | tr -d ' ') files from the git source allowlist; ignored files stay local"

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

COPYFILE_DISABLE=1 tar -cz -C "$repo" --null -T "$file_list" |
    ssh -o BatchMode=yes "$destination" "$extract_cmd"

# AppleDouble suppression is the point of COPYFILE_DISABLE, so verify it worked
# rather than assuming.
strays=$(ssh -o BatchMode=yes "$destination" "$stray_cmd")
if [ "${strays//[$'\r\n ']/}" != "0" ]; then
    echo "error: $strays AppleDouble ._* files reached the build host" >&2
    exit 1
fi

echo "==> synced clean; no AppleDouble sidecars present"
