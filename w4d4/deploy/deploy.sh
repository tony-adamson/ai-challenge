#!/usr/bin/env bash
# Выкладка w4d4 на personal-ru: исходники → сборка на сервере → перезапуск служб.
# .env не копируется: корневой ~/ai_challenge/.env на сервере создаётся руками.
# Запускать из корня репозитория или из любой папки: пути считаются от скрипта.
set -euo pipefail

HOST=personal-ru
REMOTE=ai_challenge
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

rsync -az --delete --exclude target/ --exclude data/ "$ROOT/w4d4/" "$HOST:$REMOTE/w4d4/"
rsync -az "$ROOT/.env.example" "$HOST:$REMOTE/.env.example"

ssh "$HOST" "set -e
  cd ~/$REMOTE/w4d4
  source ~/.cargo/env
  cargo build --release
  sudo systemctl restart w4d4-mcp w4d4-web
  systemctl --no-pager --lines=5 status w4d4-mcp w4d4-web"
