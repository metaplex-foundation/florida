#!/usr/bin/env bash
# Start a local validator for tests/localnet.rs.
#
# Loads this program (built with `cargo build-bpf`) at its real mainnet address and
# clones the current mainnet Token Metadata, Auction and Token Vault programs.
# Needs a *current* Solana/Agave CLI on PATH (or SOLANA_BIN pointing at its bin/):
# today's Token Metadata does not run on a 1.14-era validator.
#
# Env overrides:
#   SOLANA_BIN      directory holding solana + solana-test-validator (default: PATH)
#   METAPLEX_SO     built program (default: target/deploy/mpl_metaplex.so)
#   SOURCE_CLUSTER  cluster to clone programs from (default: mainnet-beta)
set -euo pipefail

cd "$(dirname "$0")/.."

bin() { if [ -n "${SOLANA_BIN:-}" ]; then echo "$SOLANA_BIN/$1"; else echo "$1"; fi; }
SOLANA=$(bin solana)
VALIDATOR=$(bin solana-test-validator)
SO=${METAPLEX_SO:-target/deploy/mpl_metaplex.so}
CLUSTER=${SOURCE_CLUSTER:-https://api.mainnet-beta.solana.com}
LEDGER=${LEDGER:-target/localnet-ledger}

METAPLEX=p1exdMJcjVao65QdewkaZRUnU6VPSXhus9n2GzWfh98
TOKEN_METADATA=metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s
AUCTION=auctxRXPeJoc4817jDhf4HbjnhEcr1cCXenosMhK5R8
VAULT=vau1zxA2LbssAUEF7Gpw91zMM1LvXrvpzJtmZ58rPsn

if [ ! -f "$SO" ]; then
  echo "error: $SO not found. Build it first with: cargo build-bpf" >&2
  exit 1
fi

echo "Using $("$VALIDATOR" --version)"

# Upgradeable programs and old loader-v2 programs need different clone flags.
clone_flag() {
  local owner
  owner=$("$SOLANA" account -u "$CLUSTER" "$1" --output json \
    | grep -o '"owner": *"[^"]*"' | head -1 | cut -d'"' -f4)
  if [ "$owner" = "BPFLoaderUpgradeab1e11111111111111111111111" ]; then
    echo --clone-upgradeable-program
  else
    echo --clone
  fi
}

args=(
  --reset
  --ledger "$LEDGER"
  --url "$CLUSTER"
  --bpf-program "$METAPLEX" "$SO"
)
for id in "$TOKEN_METADATA" "$AUCTION" "$VAULT"; do
  args+=("$(clone_flag "$id")" "$id")
done

# Run with mainnet's feature set (where this old program runs today) instead of
# "every feature on", if this validator version supports it.
if "$VALIDATOR" --help | grep -q -- '--clone-feature-set'; then
  args+=(--clone-feature-set)
else
  echo "note: this validator has no --clone-feature-set; running with all features enabled"
fi

# macOS defaults are too low for the validator; raise as far as allowed.
for n in 1000000 524288 262144 65536; do ulimit -n "$n" 2>/dev/null && break; done || true

echo "Starting validator (ledger: $LEDGER). Leave it running and run the tests in another terminal."
exec "$VALIDATOR" "${args[@]}"
