#!/bin/sh
# Модель и шлюз одной командой: mlx_lm.server (Qwen3.5-9B) на 127.0.0.1:8080 + шлюз на 127.0.0.1:8090.
# Ctrl+C останавливает оба. Токен — LLM_TOKEN из корневого .env (или из окружения).
set -e
cd "$(dirname "$0")/.."

REPO=models--mlx-community--Qwen3.5-9B-MLX-4bit
# Путь снапшота напрямую: с HF_HUB_OFFLINE=1 загрузка по имени репозитория падает.
SNAP=$(ls -d "${HF_HUB_CACHE:-$HOME/.cache/huggingface/hub}/$REPO"/snapshots/*/ 2>/dev/null | head -1)
[ -n "$SNAP" ] || { echo "Нет весов $REPO в кэше Hugging Face — скачай их по README w6d1."; exit 1; }
SERVER_BIN=../w6d1/.venv/bin/mlx_lm.server
[ -x "$SERVER_BIN" ] || { echo "Нет $SERVER_BIN — создай окружение по README w6d1."; exit 1; }

# Чужие процессы на наших портах не трогаем.
for PORT in 8080 8090; do
  if BUSY=$(lsof -nP -t -iTCP:$PORT -sTCP:LISTEN 2>/dev/null) && [ -n "$BUSY" ]; then
    echo "Порт $PORT занят (pid $BUSY: $(ps -o command= -p "$BUSY" | cut -c1-120)). Останови его сам и запусти снова."
    exit 1
  fi
done

cargo build --release -q
mkdir -p data
PID=""
stop() { if [ -n "$PID" ]; then kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; PID=""; fi; }
trap stop EXIT
trap 'exit 130' INT TERM

echo "== mlx_lm.server (лог data/mlx_server.log)"
HF_HUB_OFFLINE=1 "$SERVER_BIN" --model "$SNAP" --host 127.0.0.1 --port 8080 > data/mlx_server.log 2>&1 &
PID=$!
i=0
until curl -s --noproxy '*' -o /dev/null http://127.0.0.1:8080/health; do
  kill -0 "$PID" 2>/dev/null || { echo "Сервер модели завершился при старте — см. data/mlx_server.log"; exit 1; }
  i=$((i + 1)); [ $i -lt 180 ] || { echo "Сервер модели не ответил за 3 минуты"; exit 1; }
  sleep 1
done

echo "== шлюз (Ctrl+C — остановить всё)"
./target/release/w6d5
