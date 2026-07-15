#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$root/target/release/aegis-core"

if [[ ! -x "$binary" ]]; then
  cargo build --release --manifest-path "$root/Cargo.toml"
fi

iterations="${ITERATIONS:-25}"

for language in c cpp python javascript typescript go; do
  extension="$language"
  case "$language" in
    python) extension="py" ;;
    javascript) extension="js" ;;
    typescript) extension="ts" ;;
  esac

  start="$(date +%s%N)"
  for ((i = 0; i < iterations; i++)); do
    "$binary" run \
      --language "$language" \
      --file "$root/examples/hello.$extension" \
      --json >/dev/null
  done
  end="$(date +%s%N)"
  elapsed_ns=$((end - start))
  average_us=$((elapsed_ns / iterations / 1000))
  printf '%-12s %8d µs/request (%d iterations)\n' "$language" "$average_us" "$iterations"
done
