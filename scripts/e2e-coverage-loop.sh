#!/usr/bin/env bash
set -euo pipefail

ROUNDS="${ROUNDS:-10}"
OUT_DIR="${OUT_DIR:-target/e2e-coverage-loop}"
SUMMARY="$OUT_DIR/summary.tsv"

mkdir -p "$OUT_DIR"
printf "round\tbackend_lines\tbackend_functions\tfrontend_lines\tfrontend_note\n" > "$SUMMARY"

frontend_coverage() {
  if [[ ! -f package.json ]]; then
    printf "N/A\tno frontend package.json/playwright project in this workspace\n"
    return
  fi

  if ! grep -q '"test:e2e:coverage"' package.json; then
    printf "N/A\tpackage.json exists but no test:e2e:coverage script\n"
    return
  fi

  local frontend_log="$1"
  npm run test:e2e:coverage >"$frontend_log" 2>&1
  local pct
  pct="$(grep -Eo 'Lines[^0-9]*[0-9]+(\\.[0-9]+)?%' "$frontend_log" | tail -1 | grep -Eo '[0-9]+(\\.[0-9]+)?%' || true)"
  if [[ -z "$pct" ]]; then
    printf "unknown\tfrontend coverage script completed; parse %s\n" "$frontend_log"
  else
    printf "%s\tfrontend coverage script completed\n" "$pct"
  fi
}

backend_coverage() {
  local backend_log="$1"
  cargo llvm-cov --workspace --summary-only >"$backend_log" 2>&1
  awk '
    /^TOTAL[[:space:]]/ {
      print $10 "\t" $7;
      found=1;
    }
    END {
      if (!found) {
        print "unknown\tunknown";
      }
    }
  ' "$backend_log"
}

for round in $(seq 1 "$ROUNDS"); do
  round_dir="$OUT_DIR/round-$round"
  mkdir -p "$round_dir"

  read -r backend_lines backend_functions < <(backend_coverage "$round_dir/backend-llvm-cov.txt")
  read -r frontend_lines frontend_note < <(frontend_coverage "$round_dir/frontend-coverage.txt")

  printf "%s\t%s\t%s\t%s\t%s\n" \
    "$round" "$backend_lines" "$backend_functions" "$frontend_lines" "$frontend_note" \
    | tee -a "$SUMMARY"
done

echo "coverage loop summary: $SUMMARY"
