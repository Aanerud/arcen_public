#!/usr/bin/env bash
# Publication hygiene gate.
#
# Arcen is a public AGPL-3.0 repository. This check fails the build if
# something lands that should never be published: private infrastructure
# identifiers, key material, vendor SDK payloads, or a broken licence set.
#
# Usage: scripts/ci/check-publication-hygiene.sh [repo_root]
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=${1:-$(cd "$script_dir/../.." && pwd)}
cd "$repo_root"

failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# Files that legitimately discuss forbidden patterns.
#
# AGENTS.md is deliberately NOT excluded. It used to be, which meant the
# constitution — the file most likely to acquire a worked example naming a real
# host — was the one file the gate could never see. It is currently clean, so
# the exclusion bought nothing and cost coverage.
readonly self_exclusions=(
    ':(exclude)scripts/ci/check-publication-hygiene.sh'
)

check_absent() {
    local label=$1
    local pattern=$2
    shift 2
    local hits
    # -i: matching is case-insensitive. A private hostname written in capitals
    #     is exactly as disclosing as the same name in lower case, and a
    #     case-sensitive gate once reported "passed" while nine such hits sat in
    #     tracked source.
    # --untracked: new files are not in the index yet, but they WILL be in the
    #     squashed publication commit. Scanning only tracked files means the
    #     newest additions — the ones least reviewed — are the ones least
    #     checked. `--exclude-standard` still honours .gitignore, so genuinely
    #     ignored material (keys/, refference/, .claude/) stays out of scope.
    if hits=$(git grep -nIEi --untracked --exclude-standard \
        "$pattern" -- . "${self_exclusions[@]}" "$@" 2>/dev/null); then
        fail "$label"
        printf '%s\n' "$hits" | head -20 >&2
    fi
}

publishable_files() {
    git ls-files -c -o --exclude-standard
}

# 1. Private lab infrastructure must never be published.
#
#    The specific values — host names, the lab subnet, the VPN profile, the
#    prior project's name, its baseline commits, the commercial products this
#    was benchmarked against — are NOT written here.
#
#    They used to be. That was self-defeating: this file is published, so a
#    check spelling out a lab subnet and a VPN profile name in order to forbid
#    them disclosed both to anyone reading the repository. A gate that
#    leaks the secret it guards is worse than no gate, because it looks
#    diligent.
#
#    The patterns now live in a private, git-ignored denylist. Format is one
#    `label|extended-regex` pair per line; `#` starts a comment.
#
#    NOTE: git grep -E is POSIX ERE, where "\b" matches nothing at all and would
#    silently disable a check. Do not write word boundaries into the denylist.
readonly private_denylist=${ARCEN_PUBLICATION_DENYLIST:-.claude/publication-denylist.txt}

if [[ -f "$private_denylist" ]]; then
    while IFS='|' read -r label pattern; do
        [[ -z "${label// }" || "$label" == \#* ]] && continue
        [[ -z "${pattern// }" ]] && continue
        check_absent "$label" "$pattern"
    done <"$private_denylist"
elif [[ -n "${ARCEN_REQUIRE_PRIVATE_DENYLIST:-}" ]]; then
    # The publication export sets this. Releasing without the private checks
    # having run is not an acceptable outcome.
    fail "private denylist $private_denylist is missing and ARCEN_REQUIRE_PRIVATE_DENYLIST is set"
else
    # A contributor cloning the public repository has no denylist and should
    # still get a working gate, so this is a notice rather than a failure.
    echo "note: $private_denylist not present; private-infrastructure checks skipped." >&2
fi

# These are never legitimate regardless of whose infrastructure it is, so they
# stay in the published gate.
check_absent "SSH runbooks against real hosts are present" '(root|admin)@[0-9]{1,3}\.[0-9]{1,3}\.'

# 2. Key material must never be committed.
check_absent "private key material is present" '-----BEGIN [A-Z ]*PRIVATE KEY-----'

# 3. The local reference corpus must never be committed.
if publishable_files | grep -q '^refference/'; then
    fail "reference corpus files are tracked under refference/"
fi

# 4. Licence set must be intact and actually AGPL.
if [[ ! -f LICENSE ]]; then
    fail "LICENSE is missing"
elif ! grep -q 'GNU AFFERO GENERAL PUBLIC LICENSE' LICENSE; then
    fail "LICENSE is not the GNU AGPL"
fi

for required in legal/THIRD_PARTY_NOTICES.md legal/ORIGINS.md SECURITY.md SUPPORT.md; do
    [[ -f "$required" ]] || fail "$required is missing"
done

# 4b. The third-party inventory must actually match the locked dependency graph.
#
#     The notices file is exhaustive by policy, and it had drifted to covering
#     85 of the 373 packages that ship in a release artefact — while this gate
#     reported "passed", because nothing compared the file to Cargo.lock. A
#     policy no check enforces is a wish.
#
#     Skipped when cargo is unavailable so a docs-only environment can still run
#     the rest of the gate; CI has cargo.
if command -v cargo >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    if ! notices_drift=$(python3 scripts/generate-third-party-notices.py --check 2>&1); then
        fail "third-party notices do not match the locked dependency graph"
        printf '%s\n' "$notices_drift" | sed 's/^/    /' >&2
    fi
else
    echo "note: cargo or python3 unavailable; third-party inventory check skipped." >&2
fi

# 5. No proprietary vendor SDK material.
#
#    nv-codec-headers (ffnvcodec, MIT) is the approved clean-room source for the
#    NVENC declarations; the NVIDIA Video Codec SDK header is not redistributable.
#    Attribution text is NOT sufficient evidence on its own — a security review
#    found bindings that carried a clean-room comment while actually being
#    generated from the SDK. So assert a structural fact instead: the generated
#    bindings must agree with the vendored header on the API version, and must
#    not contain a symbol that only the SDK header defines.
readonly vendored_nvenc_header=third_party/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h
readonly generated_nvenc_bindings=hosts/capenc/src/nvenc_sys/windows_sys/nvEncodeAPI.rs

if [[ -f "$generated_nvenc_bindings" ]]; then
    if [[ ! -f "$vendored_nvenc_header" ]]; then
        fail "NVENC bindings exist without the vendored clean-room header"
    else
        header_major=$(sed -n 's/^#define NVENCAPI_MAJOR_VERSION *\([0-9]*\).*/\1/p' \
            "$vendored_nvenc_header" | head -1)
        binding_major=$(sed -n 's/.*NVENCAPI_MAJOR_VERSION[^=]*= *\([0-9][0-9]*\).*/\1/p' \
            "$generated_nvenc_bindings" | head -1)
        if [[ -n "$header_major" && -n "$binding_major" && "$header_major" != "$binding_major" ]]; then
            fail "NVENC bindings are API v$binding_major but the vendored clean-room header is v$header_major; the bindings were not generated from it"
        fi
        # Bindings must not declare a symbol the vendored header does not
        # define. A stale binding kept across a header change is exactly how
        # SDK-derived content survives a "clean-room" swap.
        for sdk_only_symbol in NV_ENC_CAPS_SUPPORT_MVHEVC_ENCODE NV_ENC_LOOKAHEAD_LEVEL; do
            if grep -q "$sdk_only_symbol" "$generated_nvenc_bindings" &&
                ! grep -q "$sdk_only_symbol" "$vendored_nvenc_header"; then
                fail "NVENC bindings declare $sdk_only_symbol, which the vendored header does not define"
            fi
        done

        # guid.rs is a THIRD hand-maintained file derived from the same header,
        # and until now nothing checked it. It carried a `n13.1.15.0` provenance
        # line while the vendored header was n12.1, and declared two GUIDs --
        # NV_ENC_H264_PROFILE_HIGH_10_GUID and NV_ENC_H264_PROFILE_HIGH_422_GUID
        # -- that the header does not define and that no code used. Unused
        # constants attract no compiler error and no test failure, so an
        # attribution comment was the only thing standing between the repository
        # and redistributing SDK-derived material. Compare the actual symbols.
        readonly generated_nvenc_guids=hosts/capenc/src/nvenc_sys/guid.rs
        if [[ -f "$generated_nvenc_guids" ]] && command -v python3 >/dev/null 2>&1; then
            guid_drift=$(python3 - "$vendored_nvenc_header" "$generated_nvenc_guids" <<'PYEOF'
import re
import sys

header = open(sys.argv[1]).read()
rust = open(sys.argv[2]).read()

defined = {}
for match in re.finditer(r"static\s+const\s+GUID\s+(\w+)\s*=\s*\{(.*?)\}\s*;", header, re.S):
    defined[match.group(1)] = [
        int(value, 16) for value in re.findall(r"0x[0-9a-fA-F]+", match.group(2))
    ]

declared = {}
for match in re.finditer(r"pub const (\w+): GUID = GUID \{(.*?)\};", rust, re.S):
    declared[match.group(1)] = [
        int(value, 16) for value in re.findall(r"0x[0-9a-fA-F]+", match.group(2))
    ]

for name in sorted(set(declared) - set(defined)):
    print(f"{name}: declared in guid.rs, not defined by the vendored header")
for name in sorted(set(declared) & set(defined)):
    if declared[name] != defined[name]:
        print(f"{name}: value differs from the vendored header")
PYEOF
)
            if [[ -n "$guid_drift" ]]; then
                fail "NVENC GUID constants do not match the vendored clean-room header"
                printf '%s\n' "$guid_drift" | sed 's/^/    /' >&2
            fi
        fi

        # All three derived files cite the upstream tag in a comment, and the
        # vendored README records which tag was actually vendored. Those four
        # claims drifted apart once already: guid.rs said n13.1.15.0 while the
        # header, version.rs and nvEncodeAPI.rs said n12.1.14.1. A provenance
        # comment nobody checks is decoration, so check it.
        nvenc_tags=$(
            {
                sed -n 's/.*[Tt]ag[^n]*\(n[0-9][0-9.]*\).*/\1/p' \
                    third_party/nv-codec-headers/README.md | head -1
                for derived in "$generated_nvenc_bindings" \
                    hosts/capenc/src/nvenc_sys/version.rs \
                    "$generated_nvenc_guids"; do
                    if [[ -f "$derived" ]]; then
                        sed -n 's/.*`\(n[0-9][0-9.]*\)`.*/\1/p' "$derived" | head -1
                    fi
                done
            } | sort -u
        )
        if [[ $(printf '%s\n' "$nvenc_tags" | grep -c .) -gt 1 ]]; then
            fail "NVENC provenance tags disagree across the vendored header and its derived files"
            printf '%s\n' "$nvenc_tags" | sed 's/^/    /' >&2
        fi

        # Struct-version constants are transcribed by hand because
        # NVENCAPI_STRUCT_VERSION is a function-like macro bindgen will not
        # expand. Drift here compiles perfectly and then fails on real hardware
        # with NV_ENC_ERR_INVALID_VERSION -- which has happened twice: once from
        # constants left at the previous API version, and once from dropping the
        # `1u<<31` high bit while transcribing. Compare against the header.
        if [[ -f hosts/capenc/src/nvenc_sys/version.rs ]] && command -v python3 >/dev/null 2>&1; then
            version_drift=$(python3 - "$vendored_nvenc_header" hosts/capenc/src/nvenc_sys/version.rs <<'PYEOF'
import re
import sys

flag = re.compile(r"1u?<<31")
struct_version = re.compile(r"NVENCAPI_STRUCT_VERSION\((\d+)\)")


def parse(text, pattern):
    found = {}
    for match in re.finditer(pattern, text, re.M):
        numbers = struct_version.findall(match.group(2))
        if numbers:
            found[match.group(1)] = (
                numbers[0],
                bool(flag.search(match.group(2).replace(" ", ""))),
            )
    return found


header = parse(
    open(sys.argv[1]).read(),
    r"^#define\s+(NV_[A-Z0-9_]*_VER|NV_ENCODE_API_FUNCTION_LIST_VER)\s+(.+?)\s*$",
)
rust = parse(open(sys.argv[2]).read(), r"^pub const ([A-Z0-9_]+): u32 = (.+);$")
for name in sorted(header):
    if rust.get(name) != header[name]:
        print(f"{name}: header={header[name]} version.rs={rust.get(name)}")
PYEOF
)
            if [[ -n "$version_drift" ]]; then
                fail "NVENC struct-version constants do not match the vendored header:"
                printf '%s\n' "$version_drift" | sed 's/^/    /' >&2
            fi
        fi
    fi
fi

# 6. The removed commercial licensing stack must not come back.
if publishable_files | grep -qE '^(shared/licensing|tools/license-issuer)/'; then
    fail "the commercial licensing stack has reappeared; Arcen is AGPL and unlicensed by design"
fi

# 7. Every first-party crate must declare AGPL-3.0-only. A crate that silently
#    ships under a permissive licence (or none at all) would let the copyleft be
#    bypassed for that component. Both have happened in this repo before.
if command -v cargo >/dev/null 2>&1; then
    mismatched=$(cargo metadata --format-version 1 --no-deps 2>/dev/null |
        python3 -c '
import json
import sys

try:
    meta = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for package in meta.get("packages", []):
    declared = package.get("license") or "<none>"
    if declared != "AGPL-3.0-only":
        sys.stdout.write(package["name"] + ": " + declared + "\n")
' || true)
    if [[ -n "$mismatched" ]]; then
        fail "first-party crates do not declare AGPL-3.0-only:"
        printf '%s\n' "$mismatched" | sed 's/^/    /' >&2
    fi
fi

if [[ $failures -gt 0 ]]; then
    echo >&2
    echo "$failures publication-hygiene check(s) failed." >&2
    exit 1
fi

# Self-test: prove the matching machinery actually detects something. A gate
# that silently matches nothing is worse than no gate at all, and an earlier
# revision of this script did exactly that by using "\b" in a POSIX ERE.
if ! git grep -qIE 'AFFERO' -- LICENSE; then
    echo "FAIL: hygiene self-test could not find a known-present string;" >&2
    echo "      the search machinery is broken and results cannot be trusted." >&2
    exit 1
fi

echo "Publication hygiene checks passed."
