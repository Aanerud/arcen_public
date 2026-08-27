#!/usr/bin/env bash
set -euo pipefail

read -r -a compiler <<<"${SYNTH_CC:-clang}"
read -r -a archive_command <<<"${SYNTH_AR:-ar}"
target_style="${SYNTH_TARGET_STYLE:-clang}"
verifier="$(cd "$(dirname "$0")/.." && pwd)/scripts/verify-openh264-assembly.sh"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/arcen-openh264-assembly-test.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

compile_object() {
  target="$1"
  source="$2"
  output="$3"
  if [[ "$target_style" == "zig" ]]; then
    "${compiler[@]}" -target "$target" -c "$source" -o "$output"
  else
    "${compiler[@]}" --target="$target" -c "$source" -o "$output"
  fi
}

make_archive_family() {
  root="$1"
  family="$2"
  object="$3"
  output="$root/release/build/openh264-sys2-synthetic/out"
  mkdir -p "$output"
  "${archive_command[@]}" rcs "$output/${family}_common.a" "$object"
  "${archive_command[@]}" rcs "$output/${family}_processing.a" "$object"
  "${archive_command[@]}" rcs "$output/${family}_decoder.a" "$object"
  "${archive_command[@]}" rcs "$output/${family}_encoder.a" "$object"
}

make_archive_set() {
  make_archive_family "$1" libopenh264 "$2"
}

expect_rejected() {
  architecture="$1"
  root="$2"
  label="$3"
  if OPENH264_ASSEMBLY_ARCH="$architecture" bash "$verifier" "$root" >/dev/null 2>&1; then
    echo "assembly verifier accepted $label" >&2
    exit 1
  fi
}

cat >"$temporary_directory/x86.s" <<'EOF'
.text
.globl WelsDctT4_sse2
WelsDctT4_sse2:
  ret
EOF
cat >"$temporary_directory/aarch64.s" <<'EOF'
.text
.globl WelsCopy16x16_AArch64_neon
WelsCopy16x16_AArch64_neon:
  ret
EOF
cat >"$temporary_directory/missing-symbol.s" <<'EOF'
.text
.globl unrelated_symbol
unrelated_symbol:
  ret
EOF
cat >"$temporary_directory/undefined-symbol.s" <<'EOF'
.text
.globl unrelated_symbol
unrelated_symbol:
  .quad WelsDctT4_sse2
EOF
cat >"$temporary_directory/fat-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O universal binary with 2 architectures: [x86_64:Mach-O 64-bit x86_64 object] [arm64:Mach-O 64-bit arm64 object]'
EOF
chmod +x "$temporary_directory/fat-file"
cat >"$temporary_directory/flagged-macho-x86-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O 64-bit x86_64 object, flags:<|SUBSECTIONS_VIA_SYMBOLS>'
EOF
cat >"$temporary_directory/flagged-macho-i386-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O i386 object, flags:<|SUBSECTIONS_VIA_SYMBOLS>'
EOF
cat >"$temporary_directory/flagged-macho-arm64-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O 64-bit arm64 object, flags:<|SUBSECTIONS_VIA_SYMBOLS>'
EOF
cat >"$temporary_directory/mixed-macho-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O 64-bit x86_64 object, arm64 object'
EOF
cat >"$temporary_directory/arbitrary-macho-file" <<'EOF'
#!/usr/bin/env bash
echo 'Mach-O 64-bit x86_64 object, arbitrary payload'
EOF
chmod +x \
  "$temporary_directory/flagged-macho-x86-file" \
  "$temporary_directory/flagged-macho-i386-file" \
  "$temporary_directory/flagged-macho-arm64-file" \
  "$temporary_directory/mixed-macho-file" \
  "$temporary_directory/arbitrary-macho-file"

compile_object x86_64-linux-gnu "$temporary_directory/x86.s" "$temporary_directory/dct.o"
if [[ "$target_style" == "zig" ]]; then
  i686_target="x86-linux-gnu"
else
  i686_target="i686-linux-gnu"
fi
mkdir -p "$temporary_directory/i686"
compile_object "$i686_target" "$temporary_directory/x86.s" "$temporary_directory/i686/dct.o"
compile_object aarch64-linux-gnu "$temporary_directory/aarch64.s" \
  "$temporary_directory/copy_mb_aarch64_neon.o"
if [[ "$target_style" == "zig" ]]; then
  macos_x86_target="x86_64-macos"
  macos_arm_target="aarch64-macos"
else
  macos_x86_target="x86_64-apple-darwin"
  macos_arm_target="aarch64-apple-darwin"
fi
mkdir -p "$temporary_directory/macos-x86" "$temporary_directory/macos-arm"
compile_object "$macos_x86_target" "$temporary_directory/x86.s" \
  "$temporary_directory/macos-x86/dct.o"
compile_object "$macos_arm_target" "$temporary_directory/aarch64.s" \
  "$temporary_directory/macos-arm/copy_mb_aarch64_neon.o"
mkdir -p "$temporary_directory/missing"
compile_object x86_64-linux-gnu "$temporary_directory/missing-symbol.s" \
  "$temporary_directory/missing/dct.o"
mkdir -p "$temporary_directory/undefined"
compile_object x86_64-linux-gnu "$temporary_directory/undefined-symbol.s" \
  "$temporary_directory/undefined/dct.o"

make_archive_set "$temporary_directory/valid-x86" "$temporary_directory/dct.o"
OPENH264_ASSEMBLY_ARCH=x86_64 bash "$verifier" "$temporary_directory/valid-x86"

make_archive_set "$temporary_directory/valid-i686" "$temporary_directory/i686/dct.o"
OPENH264_ASSEMBLY_ARCH=i686 bash "$verifier" "$temporary_directory/valid-i686"

make_archive_set "$temporary_directory/valid-arm64" \
  "$temporary_directory/copy_mb_aarch64_neon.o"
OPENH264_ASSEMBLY_ARCH=aarch64 bash "$verifier" "$temporary_directory/valid-arm64"

make_archive_set "$temporary_directory/valid-macos-x86" "$temporary_directory/macos-x86/dct.o"
mach_nm="${OPENH264_MACHO_NM:-${OPENH264_NM:-llvm-nm}}"
OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=x86_64 \
  bash "$verifier" "$temporary_directory/valid-macos-x86"

make_archive_set "$temporary_directory/valid-macos-arm" \
  "$temporary_directory/macos-arm/copy_mb_aarch64_neon.o"
OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=aarch64 \
  bash "$verifier" "$temporary_directory/valid-macos-arm"

OPENH264_FILE="$temporary_directory/flagged-macho-x86-file" \
  OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=x86_64 \
  bash "$verifier" "$temporary_directory/valid-macos-x86"
OPENH264_FILE="$temporary_directory/flagged-macho-i386-file" \
  OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=i686 \
  bash "$verifier" "$temporary_directory/valid-i686"
OPENH264_FILE="$temporary_directory/flagged-macho-arm64-file" \
  OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=aarch64 \
  bash "$verifier" "$temporary_directory/valid-macos-arm"
if OPENH264_FILE="$temporary_directory/mixed-macho-file" \
  OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=x86_64 \
  bash "$verifier" "$temporary_directory/valid-macos-x86" >/dev/null 2>&1; then
  echo "assembly verifier accepted a mixed-architecture Mach-O suffix" >&2
  exit 1
fi
if OPENH264_FILE="$temporary_directory/arbitrary-macho-file" \
  OPENH264_NM="$mach_nm" OPENH264_ASSEMBLY_ARCH=x86_64 \
  bash "$verifier" "$temporary_directory/valid-macos-x86" >/dev/null 2>&1; then
  echo "assembly verifier accepted an arbitrary Mach-O suffix" >&2
  exit 1
fi

mkdir -p "$temporary_directory/fake"
printf 'not an object\n' >"$temporary_directory/fake/dct.o"
make_archive_set "$temporary_directory/fake-name" "$temporary_directory/fake/dct.o"
expect_rejected x86_64 "$temporary_directory/fake-name" "a filename-only fake"

mkdir -p "$temporary_directory/wrong"
cp "$temporary_directory/copy_mb_aarch64_neon.o" "$temporary_directory/wrong/dct.o"
make_archive_set "$temporary_directory/wrong-architecture" "$temporary_directory/wrong/dct.o"
expect_rejected x86_64 "$temporary_directory/wrong-architecture" "a wrong-architecture object"

make_archive_set "$temporary_directory/missing-symbol" "$temporary_directory/missing/dct.o"
expect_rejected x86_64 "$temporary_directory/missing-symbol" "an object without an upstream symbol"

make_archive_set "$temporary_directory/undefined-symbol" "$temporary_directory/undefined/dct.o"
expect_rejected x86_64 "$temporary_directory/undefined-symbol" "an undefined upstream symbol"

make_archive_set "$temporary_directory/fat-object" "$temporary_directory/dct.o"
if OPENH264_ASSEMBLY_ARCH=x86_64 OPENH264_FILE="$temporary_directory/fat-file" \
  bash "$verifier" "$temporary_directory/fat-object" >/dev/null 2>&1; then
  echo "assembly verifier accepted a universal/fat Mach-O object description" >&2
  exit 1
fi

mkdir -p "$temporary_directory/plain"
cp "$temporary_directory/missing/dct.o" "$temporary_directory/plain/plain.o"
make_archive_set "$temporary_directory/partial-component" "$temporary_directory/dct.o"
partial_output="$temporary_directory/partial-component/release/build/openh264-sys2-synthetic/out"
rm "$partial_output/libopenh264_decoder.a"
"${archive_command[@]}" rcs "$partial_output/libopenh264_decoder.a" \
  "$temporary_directory/plain/plain.o"
expect_rejected x86_64 "$temporary_directory/partial-component" \
  "a component without assembly evidence"

mkdir -p "$temporary_directory/mixed"
make_archive_set "$temporary_directory/mixed/valid" "$temporary_directory/dct.o"
cp "$temporary_directory/missing/dct.o" "$temporary_directory/missing-symbol.o"
make_archive_set "$temporary_directory/mixed/empty" "$temporary_directory/missing-symbol.o"
expect_rejected x86_64 "$temporary_directory/mixed" "a mixed valid and assembly-empty archive tree"

make_archive_set "$temporary_directory/mixed-members" "$temporary_directory/dct.o"
"${archive_command[@]}" rcs \
  "$temporary_directory/mixed-members/release/build/openh264-sys2-synthetic/out/libopenh264_processing.a" \
  "$temporary_directory/copy_mb_aarch64_neon.o"
expect_rejected x86_64 "$temporary_directory/mixed-members" "a mixed-architecture component archive"

make_archive_set "$temporary_directory/duplicate-members" "$temporary_directory/dct.o"
"${archive_command[@]}" q \
  "$temporary_directory/duplicate-members/release/build/openh264-sys2-synthetic/out/libopenh264_processing.a" \
  "$temporary_directory/wrong/dct.o"
expect_rejected x86_64 "$temporary_directory/duplicate-members" \
  "a duplicate-name mixed-architecture component archive"

make_archive_set "$temporary_directory/empty-component" "$temporary_directory/dct.o"
"${archive_command[@]}" d \
  "$temporary_directory/empty-component/release/build/openh264-sys2-synthetic/out/libopenh264_decoder.a" \
  dct.o
expect_rejected x86_64 "$temporary_directory/empty-component" "an empty component archive"

make_archive_set "$temporary_directory/masked-commonless/valid" "$temporary_directory/dct.o"
commonless_output="$temporary_directory/masked-commonless/partial/release/build/openh264-sys2-synthetic/out"
mkdir -p "$commonless_output"
"${archive_command[@]}" rcs "$commonless_output/libopenh264_processing.a" \
  "$temporary_directory/dct.o"
expect_rejected x86_64 "$temporary_directory/masked-commonless" \
  "a common-less partial archive root beside a valid set"

make_archive_set "$temporary_directory/parallel-family" "$temporary_directory/dct.o"
make_archive_family "$temporary_directory/parallel-family" openh264 \
  "$temporary_directory/missing/dct.o"
expect_rejected x86_64 "$temporary_directory/parallel-family" \
  "a valid family beside an invalid parallel family"

newline_mask_root="$temporary_directory/newline-mask"
make_archive_set "$newline_mask_root/valid-a" "$temporary_directory/dct.o"
make_archive_set "$newline_mask_root/valid-b" "$temporary_directory/dct.o"
valid_a_output="$newline_mask_root/valid-a/release/build/openh264-sys2-synthetic/out"
valid_b_output="$newline_mask_root/valid-b/release/build/openh264-sys2-synthetic/out"
newline_output="${valid_a_output}"$'\n'"${valid_b_output}"
mkdir -p "$newline_output"
cp "$valid_a_output/libopenh264_processing.a" \
  "$newline_output/libopenh264_processing.a"
expect_rejected x86_64 "$newline_mask_root" \
  "a newline-bearing partial root masked by valid path fragments"

echo "Verified per-component OpenH264 assembly proof supports flagged thin Mach-O and i686 while rejecting fat/mixed/arbitrary Mach-O, fake-name, wrong-architecture, undefined, missing-symbol, partial-component, incomplete-family, parallel-family, newline-root masking, mixed-set, mixed-member, duplicate-name, and empty-component evidence."
