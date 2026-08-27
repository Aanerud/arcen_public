#!/usr/bin/env bash
set -euo pipefail

target_root="${1:-target}"
architecture="${OPENH264_ASSEMBLY_ARCH:-$(uname -m)}"
read -r -a archive_command <<<"${OPENH264_AR:-ar}"
read -r -a file_command <<<"${OPENH264_FILE:-file}"
read -r -a nm_command <<<"${OPENH264_NM:-nm}"
command -v "${archive_command[0]}" >/dev/null
command -v "${file_command[0]}" >/dev/null
command -v "${nm_command[0]}" >/dev/null

case "$architecture" in
  x86_64|amd64|AMD64)
    assembly_kind="NASM"
    member_pattern='(^|/)(cpuid|coeff|dct|quant|score|vaa)(_[[:alnum:]_]+)?\.o$'
    object_pattern='^(ELF 64-bit LSB relocatable, x86-64(,.*)?|Mach-O 64-bit (object x86_64|x86_64 object)(, flags:<[[:alnum:]_|-]+>)?)$'
    symbol_pattern='[[:space:]][Tt][[:space:]]+_?(Wels(BlockZero|CPU|Dct|Hadamard|IDct|Quant|Dequant|Scan|SampleSad|SampleSatd|Vaa|VAACalc)|AnalysisVaaInfoIntra|CavlcParamCal|MdInterAnalysisVaaInfo|SampleVariance|VAACalc)[[:alnum:]_]*$'
    ;;
  x86|i386|i486|i586|i686)
    assembly_kind="NASM"
    member_pattern='(^|/)(cpuid|coeff|dct|quant|score|vaa)(_[[:alnum:]_]+)?\.o$'
    object_pattern='^(ELF 32-bit LSB relocatable, Intel (80386|i386)(,.*)?|Mach-O (object i386|i386 object)(, flags:<[[:alnum:]_|-]+>)?)$'
    symbol_pattern='[[:space:]][Tt][[:space:]]+_?(Wels(BlockZero|CPU|Dct|Hadamard|IDct|Quant|Dequant|Scan|SampleSad|SampleSatd|Vaa|VAACalc)|AnalysisVaaInfoIntra|CavlcParamCal|MdInterAnalysisVaaInfo|SampleVariance|VAACalc)[[:alnum:]_]*$'
    ;;
  arm64|ARM64|aarch64|AARCH64)
    assembly_kind="AArch64"
    member_pattern='(^|/)([^/]*-)?(adaptive_quantization|block_add|copy_mb|deblocking|down_sample|expand_picture|intra_pred|intra_pred_common|intra_pred_sad_3_opt|mc|memory|pixel|pixel_sad|reconstruct|svc_motion_estimation|vaa_calc)_aarch64_neon\.o$'
    object_pattern='^(ELF 64-bit LSB relocatable, ARM aarch64(,.*)?|Mach-O 64-bit (object arm64|arm64 object)(, flags:<[[:alnum:]_|-]+>)?)$'
    symbol_pattern='[[:space:]][Tt][[:space:]]+_?(Wels|Mc|VAACalc|Sample|Pixel|Intra|Deblocking|Expand|Dyadic|SumOf8x8Block|Hadamard)[[:alnum:]_]*_AArch64_neon$'
    ;;
  *)
    echo "unsupported OpenH264 assembly verification architecture: $architecture" >&2
    exit 1
    ;;
esac

archive_set_count=0
verified_object_count=0
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/arcen-openh264-assembly.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM
output_directories=()
output_directory_count=0
while IFS= read -r -d '' archive_path; do
  output_dir="${archive_path%/*}"
  output_dir_seen=false
  for ((output_directory_index = 0; output_directory_index < output_directory_count; output_directory_index++)); do
    known_output_dir="${output_directories[$output_directory_index]}"
    if [[ "$known_output_dir" == "$output_dir" ]]; then
      output_dir_seen=true
      break
    fi
  done
  if [[ "$output_dir_seen" == false ]]; then
    output_directories[$output_directory_count]="$output_dir"
    output_directory_count=$((output_directory_count + 1))
  fi
done < <(
  find "$target_root" -type f \
    \( \
      -name 'libopenh264_common.a' -o \
      -name 'libopenh264_processing.a' -o \
      -name 'libopenh264_decoder.a' -o \
      -name 'libopenh264_encoder.a' -o \
      -name 'openh264_common.a' -o \
      -name 'openh264_processing.a' -o \
      -name 'openh264_decoder.a' -o \
      -name 'openh264_encoder.a' \
    \) \
    -path '*/build/openh264-sys2-*/out/*' \
    -print0
)

for ((output_directory_index = 0; output_directory_index < output_directory_count; output_directory_index++)); do
  output_dir="${output_directories[$output_directory_index]}"
  archive_set_count=$((archive_set_count + 1))
  archive_family=""
  family_count=0
  for family in libopenh264 openh264; do
    family_component_count=0
    for component in common processing decoder encoder; do
      if [[ -f "$output_dir/${family}_${component}.a" ]]; then
        family_component_count=$((family_component_count + 1))
      fi
    done
    if [[ "$family_component_count" -eq 0 ]]; then
      continue
    fi
    if [[ "$family_component_count" -ne 4 ]]; then
      echo "incomplete OpenH264 $family archive family in $output_dir: found $family_component_count of 4 components" >&2
      exit 1
    fi
    archive_family="$family"
    family_count=$((family_count + 1))
  done
  if [[ "$family_count" -ne 1 ]]; then
    echo "ambiguous OpenH264 archive families in $output_dir: found $family_count complete families" >&2
    exit 1
  fi

  for component in common processing decoder encoder; do
    archive="$output_dir/${archive_family}_${component}.a"

    raw_member_list="$temporary_directory/raw-members-$archive_set_count-$component.txt"
    member_list="$temporary_directory/members-$archive_set_count-$component.txt"
    "${archive_command[@]}" t "$archive" >"$raw_member_list"
    awk '
      $0 != "__.SYMDEF" &&
      $0 != "__.SYMDEF SORTED" &&
      $0 != "/" &&
      $0 != "//" {
        print
      }
    ' "$raw_member_list" >"$member_list"
    duplicate_member="$(
      awk 'NF && seen[$0]++ { print; exit }' "$member_list"
    )"
    if [[ -n "$duplicate_member" ]]; then
      echo "OpenH264 archive $archive contains duplicate member name: $duplicate_member" >&2
      exit 1
    fi

    component_member_count=0
    component_object_count=0
    component_verified_object_count=0
    while IFS= read -r member; do
      if [[ -z "$member" ]]; then
        continue
      fi
      component_member_count=$((component_member_count + 1))
      if ! grep -Eiq '\.o$' <<<"$member"; then
        echo "OpenH264 archive $archive contains unexpected non-object member: $member" >&2
        exit 1
      fi
      object="$temporary_directory/object-$archive_set_count-$component-$component_object_count.o"
      "${archive_command[@]}" p "$archive" "$member" >"$object"
      description="$("${file_command[@]}" -b "$object")"
      if grep -Eiq '(universal binary|fat (binary|file)|Mach-O universal)' <<<"$description"; then
        echo "OpenH264 archive member $member is a universal/fat object, expected only $architecture: $description" >&2
        exit 1
      fi
      if ! grep -Eiq "$object_pattern" <<<"$description"; then
        echo "OpenH264 archive member $member has wrong object format for $architecture: $description" >&2
        exit 1
      fi
      component_object_count=$((component_object_count + 1))
      if ! grep -Eiq "$member_pattern" <<<"$member"; then
        continue
      fi
      symbols="$("${nm_command[@]}" -g "$object")"
      if ! grep -Eq "$symbol_pattern" <<<"$symbols"; then
        echo "OpenH264 assembly candidate $member has no expected upstream $assembly_kind symbol" >&2
        exit 1
      fi
      verified_object_count=$((verified_object_count + 1))
      component_verified_object_count=$((component_verified_object_count + 1))
    done <"$member_list"
    if [[ "$component_member_count" -eq 0 ]]; then
      echo "incomplete OpenH264 archive set in $output_dir: $component archive is empty" >&2
      exit 1
    fi
    if [[ "$component_verified_object_count" -eq 0 ]]; then
      echo "OpenH264 $component archive in $output_dir contains no verified $assembly_kind object evidence" >&2
      exit 1
    fi
  done
done

if [[ "$archive_set_count" -eq 0 ]]; then
  echo "no source-built OpenH264 archive set found under $target_root" >&2
  exit 1
fi
if [[ "$verified_object_count" -eq 0 ]]; then
  echo "OpenH264 archives contain no verified $assembly_kind object evidence" >&2
  exit 1
fi

echo "Verified $verified_object_count $assembly_kind object(s) in $archive_set_count source-built OpenH264 archive set(s) for $architecture."
