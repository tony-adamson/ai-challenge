#!/bin/sh
# N параллельных запросов к шлюзу: код ответа и время каждого. Видно очередь (200 с растущим временем),
# переполнение (503) и лимит частоты (429 — при повторном запуске в течение минуты).
# Использование: sh scripts/load.sh [N] [URL]   (по умолчанию 6 и http://127.0.0.1:8090)
# Токен — LLM_TOKEN из окружения, иначе из корневого .env (как у шлюза).
N=${1:-6}
URL=${2:-http://127.0.0.1:8090}
ENV_FILE="$(dirname "$0")/../../.env"
[ -n "$LLM_TOKEN" ] || [ ! -f "$ENV_FILE" ] || LLM_TOKEN=$(sed -n 's/^LLM_TOKEN=//p' "$ENV_FILE" | tail -1)
[ -n "$LLM_TOKEN" ] || { echo "Нет токена: добавь строку LLM_TOKEN=<пароль> в корневой .env"; exit 1; }

echo "$N параллельных запросов → $URL/v1/chat/completions"
i=1
while [ "$i" -le "$N" ]; do
  (
    BODY="{\"messages\":[{\"role\":\"user\",\"content\":\"Назови одно интересное число и объясни его одним предложением. Запрос $i.\"}],\"max_tokens\":60}"
    OUT=$(curl -s --noproxy '*' -o /dev/null -w '%{http_code} %{time_total}' -X POST "$URL/v1/chat/completions" \
      -H "Authorization: Bearer $LLM_TOKEN" -H 'Content-Type: application/json' -d "$BODY")
    printf 'запрос %-2s → код %s, %s с\n' "$i" "${OUT% *}" "${OUT#* }"
  ) &
  i=$((i + 1))
done
wait
echo "Состояние после нагрузки:"
curl -s --noproxy '*' "$URL/health"; echo
