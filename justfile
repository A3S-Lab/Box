# A3S Box - Justfile

default:
    @just --list

# AI-powered commit message
cz:
    @bash .scripts/generate-commit-message.sh

# ============================================================================
# Build
# ============================================================================

# Build all (Rust + SDKs)
build:
    cd src && cargo build --workspace
    just sdk-ts build
    just sdk-python build

# Build release
release:
    cd src && cargo build --workspace --release

# ============================================================================
# Test (unified command with progress display)
# ============================================================================

# Run all tests with progress display and module breakdown
test:
    #!/usr/bin/env bash
    set -e

    # Colors
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    DIM='\033[2m'
    RESET='\033[0m'

    # Counters
    TOTAL_PASSED=0
    TOTAL_FAILED=0
    TOTAL_IGNORED=0
    CRATES_TESTED=0
    CRATES_FAILED=0

    print_header() {
        echo ""
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        echo -e "${BOLD}  $1${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    }

    # Extract module test counts from cargo test output
    extract_module_counts() {
        local output="$1"
        # Parse "test module::submodule::test_name ... ok" lines
        # Group by top-level module and count
        echo "$output" | grep -E "^test .+::.+ \.\.\. ok$" | \
            sed 's/^test \([^:]*\)::.*/\1/' | \
            sort | uniq -c | sort -rn | \
            while read count module; do
                printf "      ${DIM}%-20s %3d tests${RESET}\n" "$module" "$count"
            done
    }

    run_tests() {
        local crate=$1
        local display_name=$2
        local extra_args=${3:-""}

        CRATES_TESTED=$((CRATES_TESTED + 1))
        echo -ne "${CYAN}▶${RESET} ${BOLD}$display_name${RESET} "

        # Run tests and capture output
        if OUTPUT=$(cd src && cargo test -p "$crate" --lib $extra_args 2>&1); then
            TEST_EXIT=0
        else
            TEST_EXIT=1
        fi

        # Extract test results
        RESULT_LINE=$(echo "$OUTPUT" | grep -E "^test result:" | tail -1)
        if [ -n "$RESULT_LINE" ]; then
            PASSED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo "0")
            FAILED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo "0")
            IGNORED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ ignored' | grep -oE '[0-9]+' || echo "0")

            TOTAL_PASSED=$((TOTAL_PASSED + PASSED))
            TOTAL_FAILED=$((TOTAL_FAILED + FAILED))
            TOTAL_IGNORED=$((TOTAL_IGNORED + IGNORED))

            if [ "$FAILED" -gt 0 ]; then
                echo -e "${RED}✗${RESET} ${DIM}$PASSED passed, $FAILED failed${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
                echo "$OUTPUT" | grep -E "^test .* FAILED$" | sed 's/^/    /'
            else
                echo -e "${GREEN}✓${RESET} ${DIM}$PASSED passed${RESET}"
                # Show module breakdown for crates with many tests
                if [ "$PASSED" -gt 10 ]; then
                    extract_module_counts "$OUTPUT"
                fi
            fi
        else
            # No tests found or compilation error
            if echo "$OUTPUT" | grep -q "error\[E"; then
                echo -e "${RED}✗${RESET} ${DIM}compile error${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
                echo "$OUTPUT" | grep -E "^error" | head -3 | sed 's/^/    /'
            elif [ "$TEST_EXIT" -ne 0 ]; then
                echo -e "${RED}✗${RESET} ${DIM}failed${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
            else
                echo -e "${YELLOW}○${RESET} ${DIM}no tests${RESET}"
            fi
        fi
    }

    print_header "🧪 A3S Box Test Suite"
    echo ""

    # Test each crate
    run_tests "a3s-box-core"    "core"
    run_tests "a3s-box-queue"   "queue"
    run_tests "a3s-box-code"    "code"
    run_tests "a3s-box-runtime" "runtime"

    # Summary
    echo ""
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"

    if [ "$CRATES_FAILED" -gt 0 ]; then
        echo -e "  ${RED}${BOLD}✗ FAILED${RESET}  ${GREEN}$TOTAL_PASSED passed${RESET}  ${RED}$TOTAL_FAILED failed${RESET}  ${YELLOW}$TOTAL_IGNORED ignored${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        exit 1
    else
        echo -e "  ${GREEN}${BOLD}✓ PASSED${RESET}  ${GREEN}$TOTAL_PASSED passed${RESET}  ${YELLOW}$TOTAL_IGNORED ignored${RESET}  ${DIM}($CRATES_TESTED crates)${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    fi
    echo ""

# Run tests without progress (raw cargo output)
test-raw:
    cd src && cargo test -p a3s-box-code -p a3s-box-core -p a3s-box-queue -p a3s-box-runtime --lib

# Run tests with verbose output
test-v:
    cd src && cargo test -p a3s-box-code -p a3s-box-core -p a3s-box-queue -p a3s-box-runtime --lib -- --nocapture

# ============================================================================
# Test Subsets
# ============================================================================

# Test a3s-box-code
test-code:
    cd src && cargo test -p a3s-box-code --lib

# Test queue and HITL
test-queue:
    cd src && cargo test -p a3s-box-code --lib -- queue::tests hitl::tests

# Test permissions
test-permissions:
    cd src && cargo test -p a3s-box-code --lib -- permissions::tests

# Test skill system
test-skills:
    cd src && cargo test -p a3s-box-code --lib -- skill_loader::tests

# Test tools (builtin + dynamic)
test-tools:
    cd src && cargo test -p a3s-box-code --lib -- tools::

# Test a3s-box-core
test-core:
    cd src && cargo test -p a3s-box-core --lib

# Test a3s-box-runtime (check only, requires libkrun for actual tests)
test-runtime:
    cd src && A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime -p a3s-box-shim
    cd src && A3S_DEPS_STUB=1 cargo clippy -p a3s-box-runtime -p a3s-box-shim -- -D warnings
    @echo "✓ Runtime compilation passed (actual tests require libkrun)"

# ============================================================================
# Coverage (requires: cargo install cargo-llvm-cov, brew install lcov)
# ============================================================================

# Test with coverage - shows real-time test progress + module coverage
test-cov:
    #!/usr/bin/env bash
    set -e

    # Colors
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    DIM='\033[2m'
    RESET='\033[0m'

    # Clear line and move cursor
    CLEAR_LINE='\033[2K'
    MOVE_UP='\033[1A'

    # Shared temp directory for grand totals
    GRAND_TMP="/tmp/test_cov_grand_$$"
    mkdir -p "$GRAND_TMP"
    echo "0" > "$GRAND_TMP/grand_tests"
    echo "0" > "$GRAND_TMP/grand_lines"
    echo "0" > "$GRAND_TMP/grand_covered"

    print_header() {
        echo ""
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        echo -e "${BOLD}  $1${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    }

    run_cov_realtime() {
        local crate=$1
        local display_name=$2

        echo -e "${CYAN}▶${RESET} ${BOLD}$display_name${RESET}"
        echo ""

        # Temp files for tracking
        local tmp_dir="/tmp/test_cov_$$_${display_name}"
        mkdir -p "$tmp_dir"

        # Initialize module counters file
        touch "$tmp_dir/module_counts"

        # Run tests with coverage, parse output in real-time
        cd src

        # Use process substitution to read output line by line
        {
            cargo llvm-cov --lib -p "$crate" 2>&1
        } | {
            current_module=""
            module_passed=0
            total_passed=0
            total_failed=0
            declare -A module_counts 2>/dev/null || true  # May fail on bash 3

            while IFS= read -r line; do
                # Check if it's a test result line
                if [[ "$line" =~ ^test\ ([a-z_]+)::.*\.\.\.\ (ok|FAILED)$ ]]; then
                    module="${BASH_REMATCH[1]}"
                    result="${BASH_REMATCH[2]}"

                    # Update counts
                    if [ "$result" = "ok" ]; then
                        total_passed=$((total_passed + 1))
                        # Track per-module (write to file for bash 3 compat)
                        count=$(grep "^${module} " "$tmp_dir/module_counts" 2>/dev/null | awk '{print $2}' || echo "0")
                        count=$((count + 1))
                        grep -v "^${module} " "$tmp_dir/module_counts" > "$tmp_dir/module_counts.tmp" 2>/dev/null || true
                        echo "$module $count" >> "$tmp_dir/module_counts.tmp"
                        mv "$tmp_dir/module_counts.tmp" "$tmp_dir/module_counts"
                    else
                        total_failed=$((total_failed + 1))
                    fi

                    # Show progress (overwrite line)
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Running:${RESET} ${module}::... ${GREEN}${total_passed}${RESET} passed"
                    [ "$total_failed" -gt 0 ] && echo -ne " ${RED}${total_failed}${RESET} failed"

                # Check for compilation message
                elif [[ "$line" =~ ^[[:space:]]*Compiling ]]; then
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Compiling...${RESET}"

                # Check for running tests message
                elif [[ "$line" =~ ^[[:space:]]*Running ]]; then
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Running tests...${RESET}"

                # Check for coverage report lines (save for later)
                elif [[ "$line" =~ ^[a-z_]+.*\.rs[[:space:]] ]]; then
                    echo "$line" >> "$tmp_dir/coverage_lines"

                # Check for TOTAL line
                elif [[ "$line" =~ ^TOTAL ]]; then
                    echo "$line" >> "$tmp_dir/total_line"
                fi
            done

            # Save final counts
            echo "$total_passed" > "$tmp_dir/total_passed"
            echo "$total_failed" > "$tmp_dir/total_failed"
        }

        cd ..

        # Clear progress line
        echo -ne "\r${CLEAR_LINE}"

        # Read results
        total_passed=$(cat "$tmp_dir/total_passed" 2>/dev/null || echo "0")
        total_failed=$(cat "$tmp_dir/total_failed" 2>/dev/null || echo "0")

        # Show final test result
        if [ "$total_failed" -gt 0 ]; then
            echo -e "      ${RED}✗${RESET} ${total_passed} passed, ${RED}${total_failed} failed${RESET}"
        else
            echo -e "      ${GREEN}✓${RESET} ${total_passed} tests passed"
        fi
        echo ""

        # Parse coverage data and aggregate by module
        if [ -f "$tmp_dir/coverage_lines" ]; then
            awk '
            {
                file=$1; lines=$8; missed=$9
                n = split(file, parts, "/")
                if (n > 1) {
                    module = parts[1]
                } else {
                    gsub(/\.rs$/, "", file)
                    module = file
                }
                total_lines[module] += lines
                total_missed[module] += missed
            }
            END {
                for (m in total_lines) {
                    if (total_lines[m] > 0) {
                        covered = total_lines[m] - total_missed[m]
                        pct = (covered / total_lines[m]) * 100
                        printf "%s %.1f %d\n", m, pct, total_lines[m]
                    }
                }
            }' "$tmp_dir/coverage_lines" | sort -t' ' -k2 -rn > "$tmp_dir/cov_agg"

            # Display coverage results with test counts
            echo -e "      ${BOLD}Module               Tests   Coverage${RESET}"
            echo -e "      ${DIM}──────────────────────────────────────────────${RESET}"

            while read module pct lines; do
                # Find test count for this module
                tests=$(grep "^${module} " "$tmp_dir/module_counts" 2>/dev/null | awk '{print $2}' || echo "0")
                [ -z "$tests" ] && tests=0

                # Color the percentage
                num=${pct%.*}
                if [ "$num" -ge 90 ]; then
                    cov_color="${GREEN}${pct}%${RESET}"
                elif [ "$num" -ge 70 ]; then
                    cov_color="${YELLOW}${pct}%${RESET}"
                else
                    cov_color="${RED}${pct}%${RESET}"
                fi
                echo -e "      $(printf '%-18s' "$module") $(printf '%4d' "$tests")   ${cov_color} ${DIM}($lines lines)${RESET}"
            done < "$tmp_dir/cov_agg"

            # Print total and accumulate grand totals
            if [ -f "$tmp_dir/total_line" ]; then
                total_cov=$(cat "$tmp_dir/total_line" | awk '{print $4}' | tr -d '%')
                total_lines=$(cat "$tmp_dir/total_line" | awk '{print $8}')
                total_missed=$(cat "$tmp_dir/total_line" | awk '{print $9}')
                total_covered=$((total_lines - total_missed))
                echo -e "      ${DIM}──────────────────────────────────────────────${RESET}"

                num=${total_cov%.*}
                if [ "$num" -ge 90 ]; then
                    cov_color="${GREEN}${BOLD}${total_cov}%${RESET}"
                elif [ "$num" -ge 70 ]; then
                    cov_color="${YELLOW}${BOLD}${total_cov}%${RESET}"
                else
                    cov_color="${RED}${BOLD}${total_cov}%${RESET}"
                fi
                echo -e "      ${BOLD}$(printf '%-18s' "TOTAL") $(printf '%4d' "$total_passed")${RESET}   ${cov_color} ${DIM}($total_lines lines)${RESET}"

                # Save to grand totals
                echo "$display_name $total_passed $total_lines $total_covered" >> "$GRAND_TMP/crate_stats"
            fi
        fi

        # Cleanup crate tmp
        rm -rf "$tmp_dir"
        echo ""
    }

    print_header "🧪 A3S Box Test Suite with Coverage"
    echo ""

    run_cov_realtime "a3s-box-core" "core"
    run_cov_realtime "a3s-box-queue" "queue"
    run_cov_realtime "a3s-box-code" "code"
    run_cov_realtime "a3s-box-runtime" "runtime"

    # Print grand total summary
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  📊 Overall Summary${RESET}"
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo ""

    if [ -f "$GRAND_TMP/crate_stats" ]; then
        echo -e "      ${BOLD}Crate                Tests    Lines   Coverage${RESET}"
        echo -e "      ${DIM}────────────────────────────────────────────────────${RESET}"

        grand_tests=0
        grand_lines=0
        grand_covered=0

        while read crate tests lines covered; do
            grand_tests=$((grand_tests + tests))
            grand_lines=$((grand_lines + lines))
            grand_covered=$((grand_covered + covered))

            if [ "$lines" -gt 0 ]; then
                pct=$(awk "BEGIN {printf \"%.2f\", ($covered / $lines) * 100}")
            else
                pct="0.00"
            fi

            num=${pct%.*}
            if [ "$num" -ge 90 ]; then
                cov_color="${GREEN}${pct}%${RESET}"
            elif [ "$num" -ge 70 ]; then
                cov_color="${YELLOW}${pct}%${RESET}"
            else
                cov_color="${RED}${pct}%${RESET}"
            fi

            echo -e "      $(printf '%-18s' "$crate") $(printf '%5d' "$tests")   $(printf '%6d' "$lines")   ${cov_color}"
        done < "$GRAND_TMP/crate_stats"

        echo -e "      ${DIM}────────────────────────────────────────────────────${RESET}"

        # Calculate grand total percentage
        if [ "$grand_lines" -gt 0 ]; then
            grand_pct=$(awk "BEGIN {printf \"%.2f\", ($grand_covered / $grand_lines) * 100}")
        else
            grand_pct="0.00"
        fi

        num=${grand_pct%.*}
        if [ "$num" -ge 90 ]; then
            grand_cov_color="${GREEN}${BOLD}${grand_pct}%${RESET}"
        elif [ "$num" -ge 70 ]; then
            grand_cov_color="${YELLOW}${BOLD}${grand_pct}%${RESET}"
        else
            grand_cov_color="${RED}${BOLD}${grand_pct}%${RESET}"
        fi

        echo -e "      ${BOLD}$(printf '%-18s' "GRAND TOTAL") $(printf '%5d' "$grand_tests")   $(printf '%6d' "$grand_lines")${RESET}   ${grand_cov_color}"
    fi

    echo ""
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo ""

    # Cleanup grand tmp
    rm -rf "$GRAND_TMP"

# Coverage with pretty terminal output
cov:
    #!/usr/bin/env bash
    set -e
    COV_FILE="/tmp/a3s-box-coverage.lcov"
    echo "┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓"
    echo "┃                    🧪 Running Tests with Coverage                     ┃"
    echo "┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛"
    cd src && cargo llvm-cov --lib -p a3s-box-code -p a3s-box-core -p a3s-box-queue \
        --lcov --output-path "$COV_FILE" 2>&1 | grep -E "^test result"
    echo ""
    echo "┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓"
    echo "┃                         📊 Coverage Report                            ┃"
    echo "┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛"
    lcov --summary "$COV_FILE" 2>&1
    rm -f "$COV_FILE"

# Coverage for specific module
cov-module MOD:
    cd src && cargo llvm-cov --lib -p a3s-box-code -- {{MOD}}::

# Coverage with HTML report (opens in browser)
cov-html:
    cd src && cargo llvm-cov --lib -p a3s-box-code -p a3s-box-core -p a3s-box-queue --html --open

# Coverage with detailed file-by-file table
cov-table:
    cd src && cargo llvm-cov --lib -p a3s-box-code -p a3s-box-core -p a3s-box-queue

# Coverage for CI (generates lcov.info)
cov-ci:
    cd src && cargo llvm-cov --lib -p a3s-box-code -p a3s-box-core -p a3s-box-queue --lcov --output-path lcov.info

# ============================================================================
# Code Quality
# ============================================================================

# Format code
fmt:
    cd src && cargo fmt --all

# Lint (clippy)
lint:
    cd src && cargo clippy --all-targets --all-features -- -D warnings

# CI checks (fmt + lint + test)
ci:
    cd src && cargo fmt --all -- --check
    cd src && cargo clippy --all-targets --all-features -- -D warnings
    cd src && cargo test --all

# ============================================================================
# Crate Commands
# ============================================================================

core *ARGS:
    just -f src/core/justfile {{ARGS}}

runtime *ARGS:
    just -f src/runtime/justfile {{ARGS}}

code *ARGS:
    just -f src/code/justfile {{ARGS}}

queue *ARGS:
    just -f src/queue/justfile {{ARGS}}

sdk-python *ARGS:
    just -f src/sdk/python/justfile {{ARGS}}

sdk-ts *ARGS:
    just -f src/sdk/typescript/justfile {{ARGS}}

# ============================================================================
# Utilities
# ============================================================================

# Watch and rebuild
watch:
    cd src && cargo watch -x build

# Generate docs
doc:
    cd src && cargo doc --no-deps --open

# Clean artifacts
clean:
    cd src && cargo clean

