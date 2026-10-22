#!/usr/bin/env bash
# One command to get everything ready: ./start.sh
# Starts the local AI, downloads its model the first time, builds the app.
set -euo pipefail
cd "$(dirname "$0")"

MODEL="${OLLAMA_MODEL:-qwen2.5:3b}"

echo "Starting the local AI (Ollama)..."
docker compose up -d --wait ollama

if ! docker compose exec -T ollama ollama list | grep -q "^$MODEL"; then
  echo "Downloading $MODEL (about 2 GB, only the first time)..."
  docker compose exec -T ollama ollama pull "$MODEL"
fi

echo "Building the app (slow only the first time)..."
docker compose build --quiet app

mkdir -p data/inbox

cat <<EOF

Ready. Next:
  1. Save Itaú .ofx statements into data/inbox/
  2. ./finance import data/inbox
  3. ./finance summary
Everything else: ./finance --help
EOF
