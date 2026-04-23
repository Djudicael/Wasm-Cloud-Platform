#!/usr/bin/env bash
# scripts/check-log-fields.sh
# Validates that all tracing calls use the correct field names.
# Run in CI after `cargo fmt` and before `cargo clippy`.

set -euo pipefail

CRATES_DIR="crates"
ERRORS=0

# Check for inconsistent field names
# Each pattern is: "wrong_name" → "correct_name"
declare -A FIELD_CHECKS=(
    ["app ="]="app_id ="
    ["application ="]="app_id ="
    ["name ="]="app_id ="
    ["app_id =%"]="app_id = %"
    ["err ="]="error ="
    ["e ="]="error ="
    ["msg ="]="message ="
    ["duration ="]="latency_ms ="
    ["elapsed ="]="latency_ms ="
    ["latency ="]="latency_ms ="
    ["hash ="]="sha256 ="
    ["artifact_hash ="]="sha256 ="
)

echo "Checking tracing field names in $CRATES_DIR/..."

for crate_dir in "$CRATES_DIR"/*/; do
    crate_name=$(basename "$crate_dir")

    # Skip test directories
    if [[ "$crate_name" == "e2e" ]]; then
        continue
    fi

    # Find all Rust source files
    while IFS= read -r file; do
        line_num=0
        while IFS= read -r line; do
            line_num=$((line_num + 1))

            # Skip comments
            if [[ "$line" =~ ^[[:space:]]*// ]]; then
                continue
            fi

            # Check for tracing macros
            if [[ "$line" =~ tracing::(info|warn|error|debug|trace)! ]]; then
                for wrong in "${!FIELD_CHECKS[@]}"; do
                    correct="${FIELD_CHECKS[$wrong]}"
                    if [[ "$line" =~ $wrong ]]; then
                        echo "ERROR: $file:$line_num: Found '$wrong' — use '$correct' instead"
                        echo "  $line"
                        ERRORS=$((ERRORS + 1))
                    fi
                done
            fi

            # Check for eprintln!() and println!() in non-test code
            if [[ "$line" =~ eprintln! || "$line" =~ println! ]]; then
                # Allow in test functions
                if [[ ! "$line" =~ "#\[test\]" && ! "$line" =~ "fn test_" ]]; then
                    echo "WARN: $file:$line_num: Use tracing instead of eprintln!/println"
                    echo "  $line"
                fi
            fi
        done < "$file"
    done < <(find "$crate_dir" -name "*.rs" -not -path "*/tests/*")
done

if [ $ERRORS -gt 0 ]; then
    echo ""
    echo "Found $ERRORS field naming errors. Fix them before merging."
    exit 1
fi

echo "All tracing field names are consistent."
exit 0
