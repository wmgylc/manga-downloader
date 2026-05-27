#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_FILE="$PROJECT_DIR/deploy.log"
BARK_URL="${BARK_URL:-https://bark.wmgylc.top:10000/CzS6dEcWSikbSJomnfYgZT}"

notify() {
  local title="$1"
  local body="$2"
  curl -fsS --get \
    --data-urlencode "title=$title" \
    --data-urlencode "body=$body" \
    "$BARK_URL" >/dev/null 2>&1 || true
}

{
  echo "== $(date '+%Y-%m-%d %H:%M:%S') deploying manga-downloader =="
  cd "$PROJECT_DIR"
  mkdir -p data/tasks data/backups
  if [[ -f data/tasks/manga-tasks.sqlite ]]; then
    backup_path="data/backups/manga-tasks.$(date '+%Y%m%d-%H%M%S').sqlite"
    cp -p data/tasks/manga-tasks.sqlite "$backup_path"
    echo "backed up task database to $backup_path"
  elif [[ -f data/wnacg/wnacg-tasks.sqlite ]]; then
    cp -p data/wnacg/wnacg-tasks.sqlite data/tasks/manga-tasks.sqlite
    echo "migrated legacy task database from data/wnacg/wnacg-tasks.sqlite"
  fi
  export DOCKER_BUILDKIT=1
  export COMPOSE_DOCKER_CLI_BUILD=1
  docker compose -f docker-compose.cli.yml build
  docker compose -f docker-compose.cli.yml up -d
  docker compose -f docker-compose.cli.yml ps
  echo "== $(date '+%Y-%m-%d %H:%M:%S') deployment complete =="
} >>"$LOG_FILE" 2>&1

notify "manga-downloader deploy complete" "codex/add-jmcomic-support deployed on 10.10.10.206"
