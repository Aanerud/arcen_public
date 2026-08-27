#!/usr/bin/env bash

arcen_checksum_command() {
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s\n' sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        printf '%s\n' 'shasum -a 256'
    else
        echo "no SHA-256 utility is available" >&2
        return 1
    fi
}

arcen_emit_checksums() {
    local directory=$1
    local command
    command=$(arcen_checksum_command)

    (
        cd "$directory"
        while IFS= read -r -d '' file; do
            $command "$file"
        done < <(find . -type f ! -name SHA256SUMS -print0 | sort -z)
    )
}

arcen_write_checksums() {
    local directory=$1
    arcen_emit_checksums "$directory" > "$directory/SHA256SUMS"
}

arcen_check_checksums() {
    local directory=$1
    local actual

    actual=$(mktemp)
    if ! arcen_emit_checksums "$directory" > "$actual"; then
        rm -f "$actual"
        return 1
    fi
    if ! cmp -s "$directory/SHA256SUMS" "$actual"; then
        echo "$directory/SHA256SUMS does not exactly cover the staged files" >&2
        diff -u "$directory/SHA256SUMS" "$actual" >&2 || true
        rm -f "$actual"
        return 1
    fi
    rm -f "$actual"
}

arcen_checksum_digest() {
    local file=$1
    local command
    command=$(arcen_checksum_command)
    $command "$file" | awk '{print $1}'
}
