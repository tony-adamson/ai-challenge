#!/bin/sh
# Обратный туннель: порт 8090 на VPS (только 127.0.0.1, наружу его отдаёт Caddy как /llm) → шлюз на этом Mac.
# Живёт, пока открыт терминал; Ctrl+C — закрыть доступ из интернета.
exec ssh -N -i ~/.ssh/id_ed25519_personal -o IdentitiesOnly=yes -o ServerAliveInterval=30 -o ExitOnForwardFailure=yes \
  -R 127.0.0.1:8090:127.0.0.1:8090 user@91.188.212.67
