#!/usr/bin/env bash
# Deploy MockUSDC + Bridge to an EVM node (default: local Anvil) and write
# bridge/deployments/${DEPLOY_NAME:-local}.json. See script/Deploy.s.sol for env vars
# (VALIDATORS, POWERS, EPOCH, MIN_DEPOSIT, MINT_AMOUNT, DEPLOY_NAME).
set -euo pipefail
cd "$(dirname "$0")"
RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
# Anvil dev account 0 (public, well-known test key).
DEPLOYER_KEY="${DEPLOYER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
mkdir -p deployments
forge script script/Deploy.s.sol:Deploy --rpc-url "$RPC_URL" --private-key "$DEPLOYER_KEY" \
  --broadcast --slow -q >/dev/null
cat "deployments/${DEPLOY_NAME:-local}.json"
echo
