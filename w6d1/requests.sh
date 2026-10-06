#!/bin/sh
# Три запроса разной сложности к локальному mlx_lm.server (OpenAI Chat Completions).
set -eu

URL="${URL:-http://localhost:8080/v1/chat/completions}"
MODEL="mlx-community/Qwen3.5-9B-MLX-4bit"

ask() {
  echo "=== $1"
  echo "Вопрос: $2"
  start=$(date +%s)
  body=$(jq -n --arg m "$MODEL" --arg q "$2" \
    '{model: $m, messages: [{role: "user", content: $q}], max_tokens: 1024,
      chat_template_kwargs: {enable_thinking: false}}')
  resp=$(curl -sf "$URL" -H 'Content-Type: application/json' -d "$body") || {
    echo "Сервер не отвечает на $URL — запусти mlx_lm.server (см. README)." >&2
    exit 1
  }
  echo "Ответ:"
  printf '%s\n' "$resp" | jq -r '.choices[0].message.content'
  printf '%s\n' "$resp" | jq -r '"[токенов: \(.usage.completion_tokens), время: '"$(( $(date +%s) - start ))"' с]"'
  echo
}

ask "1. Простой: факт" \
  "Какая столица Австралии? Ответь одним словом."

ask "2. Средний: объяснение" \
  "Объясни в 3–4 предложениях, чем процесс отличается от потока в операционной системе."

ask "3. Сложный: рассуждение" \
  "Поезд выехал из A в 9:40 со скоростью 80 км/ч. Через 30 минут из B навстречу ему выехал второй поезд со скоростью 100 км/ч. Расстояние между A и B — 400 км. Во сколько поезда встретятся? Реши по шагам."
