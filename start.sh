#!/usr/bin/env bash
# One command to start everything: ./start.sh, then open http://localhost:8080
# Starts the local AI, downloads its model the first time, builds and starts the web page.
set -euo pipefail
cd "$(dirname "$0")"

MODEL="${OLLAMA_MODEL:-qwen2.5:3b}"
URL="http://localhost:8080"

echo "Starting the local AI (Ollama)..."
docker compose up -d --wait ollama

if ! docker compose exec -T ollama ollama list | grep -q "^$MODEL"; then
  echo "Downloading $MODEL (about 2 GB, only the first time)..."
  docker compose exec -T ollama ollama pull "$MODEL"
fi

mkdir -p data/inbox

echo "Building and starting the app (slow only the first time)..."
docker compose build --quiet web app
docker compose up -d web

for _ in $(seq 1 30); do
  curl -fsS -o /dev/null "$URL/api/months" 2>/dev/null && break
  sleep 1
done

cat <<EOF

Ready: open $URL
  - Save Itaú .ofx statements into data/inbox/ and press Import on the page
  - Or from here: ./finance import data/inbox, ./finance summary (./finance --help)
Stop everything with: docker compose down
EOF
