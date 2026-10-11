#!/bin/sh
# Бенч до/после одной командой: mlx_lm.server baseline → bench → сервер с KV-кэшем 8 бит → bench.
# Отчёты — data/bench-<профиль>-<модель>-<время>.json; вкладка «Сравнение» показывает отчёты Qwen.
# Пиковую память сервера бинарник берёт из footprint (phys_footprint_peak): RSS из ps у MLX не видит GPU-память.
# Использование: sh scripts/bench.sh [--model qwen|gemma] [--limit N]
set -e
cd "$(dirname "$0")/.."

MODEL=qwen
LIMIT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --model) MODEL="$2"; shift 2 ;;
    --limit) LIMIT="--limit $2"; shift 2 ;;
    *) echo "неизвестный аргумент: $1 (ожидается --model qwen|gemma, --limit N)"; exit 1 ;;
  esac
done
case "$MODEL" in
  qwen) REPO=models--mlx-community--Qwen3.5-9B-MLX-4bit ;;
  gemma) REPO=models--mlx-community--gemma-4-e4b-it-4bit ;;
  *) echo "--model: $MODEL (ожидается qwen или gemma)"; exit 1 ;;
esac

# Путь снапшота напрямую: с HF_HUB_OFFLINE=1 загрузка по имени репозитория падает (в кэше нет README).
SNAP=$(ls -d "${HF_HUB_CACHE:-$HOME/.cache/huggingface/hub}/$REPO"/snapshots/*/ 2>/dev/null | head -1)
[ -n "$SNAP" ] || { echo "Нет весов $REPO в кэше Hugging Face — скачай их по README w6d1."; exit 1; }
SERVER_BIN=../w6d1/.venv/bin/mlx_lm.server
[ -x "$SERVER_BIN" ] || { echo "Нет $SERVER_BIN — создай окружение по README w6d1."; exit 1; }

# Чужой сервер на 8080 не трогаем: его флаги (KV, модель) не наши, а убивать чужие процессы нельзя.
if BUSY=$(lsof -nP -t -iTCP:8080 -sTCP:LISTEN 2>/dev/null) && [ -n "$BUSY" ]; then
  echo "Порт 8080 занят (pid $BUSY: $(ps -o command= -p "$BUSY" | cut -c1-120))."
  echo "Останови этот процесс сам (например, закрой приложение) и запусти бенч снова."
  exit 1
fi

cargo build --release -q
mkdir -p data
PID=""
stop() {
  if [ -n "$PID" ]; then kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; PID=""; fi
}
trap stop EXIT
trap 'exit 130' INT TERM

run() {
  profile=$1; shift
  echo "== $profile: mlx_lm.server $* (лог data/mlx_server-$profile.log)"
  HF_HUB_OFFLINE=1 "$SERVER_BIN" --model "$SNAP" --port 8080 "$@" > "data/mlx_server-$profile.log" 2>&1 &
  PID=$!
  i=0
  until curl -s --noproxy '*' -o /dev/null http://localhost:8080/v1/models; do
    kill -0 "$PID" 2>/dev/null || { echo "Сервер завершился при старте — см. data/mlx_server-$profile.log"; exit 1; }
    i=$((i + 1)); [ $i -lt 180 ] || { echo "Сервер не ответил за 3 минуты"; exit 1; }
    sleep 1
  done
  # shellcheck disable=SC2086
  ./target/release/w6d4 bench --profile "$profile" $LIMIT
  stop
}

START=$(date +%s)
run baseline
run optimized --kv-bits 8 --quantized-kv-start 0
echo "Готово за $(( ($(date +%s) - START) / 60 )) мин. Отчёты — в data/, вкладка «Сравнение» покажет последние."
