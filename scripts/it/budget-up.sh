#!/usr/bin/env bash
# Поднимает Семейную бухгалтерию для интеграционной проверки: PostgreSQL, приложение с демо-данными
# (APP_ENV=dev), API-токен проекта «Семья» с правом write. Пишет $WORK/env.sh для it.py.
#
#   scripts/it/budget-up.sh <путь к checkout family-budget>   поднять (собирает, если нет бинарника)
#   scripts/it/budget-up.sh --down                            остановить приложение и PostgreSQL
#
# Переменные: WORK (по умолчанию /var/tmp/fms-it), PG_BIN (/usr/lib/postgresql/16/bin), PG_PORT (55432),
# APP_PORT (18080). Нужны Linux, PostgreSQL 16, Rust; на Mac — Postgres.app или контейнер (см. README).
set -euo pipefail

WORK=${WORK:-/var/tmp/fms-it}
PG_BIN=${PG_BIN:-/usr/lib/postgresql/16/bin}
PG_PORT=${PG_PORT:-55432}
APP_PORT=${APP_PORT:-18080}
PGDATA=$WORK/pgdata
PSQL=(psql -h /tmp -p "$PG_PORT" -U postgres -v ON_ERROR_STOP=1 -qtA)

as_pg() { if [ "$(id -u)" = 0 ]; then su postgres -c "$*"; else bash -c "$*"; fi; }

if [ "${1:-}" = "--down" ]; then
  [ -f "$WORK/app.pid" ] && kill "$(cat "$WORK/app.pid")" 2>/dev/null || true
  rm -f "$WORK/app.pid"
  [ -d "$PGDATA" ] && as_pg "$PG_BIN/pg_ctl -D $PGDATA stop -m fast" || true
  echo "Остановлено."
  exit 0
fi

BUDGET=${1:?"путь к checkout family-budget"}
BUDGET=$(cd "$BUDGET" && pwd)
mkdir -p "$WORK"

# 1. PostgreSQL: каждый раз чистая база, чтобы сценарии не зависели от прошлых прогонов
if [ ! -d "$PGDATA" ]; then
  mkdir -p "$PGDATA"
  [ "$(id -u)" = 0 ] && chown postgres:postgres "$PGDATA" "$WORK"
  as_pg "$PG_BIN/initdb -D $PGDATA -A trust -U postgres >/dev/null"
fi
as_pg "$PG_BIN/pg_ctl -D $PGDATA status >/dev/null 2>&1" || as_pg "$PG_BIN/pg_ctl -D $PGDATA -o '-p $PG_PORT -k /tmp' -l $PGDATA/log -w start >/dev/null"
[ -f "$WORK/app.pid" ] && kill "$(cat "$WORK/app.pid")" 2>/dev/null && sleep 1 || true
"${PSQL[@]}" -c "DROP DATABASE IF EXISTS family_budget" -c "CREATE DATABASE family_budget"

# 2. Приложение
BIN=$BUDGET/target/debug/family_budget
if [ ! -x "$BIN" ] || [ -n "${REBUILD:-}" ]; then
  (cd "$BUDGET" && cargo build -q -p family_budget)
fi
(
  cd "$BUDGET"
  APP_ENV=dev SEED_DEMO=1 BIND_ADDR=127.0.0.1:$APP_PORT APP_BASE_URL=http://127.0.0.1:$APP_PORT \
    DATABASE_URL="postgres://postgres@localhost:$PG_PORT/family_budget?host=/tmp" RATE_LIMIT_PER_MIN=100000 \
    nohup "$BIN" >"$WORK/app.log" 2>&1 &
  echo $! >"$WORK/app.pid"
)
for _ in $(seq 1 60); do
  curl -fsS -o /dev/null "http://127.0.0.1:$APP_PORT/" 2>/dev/null && break
  sleep 1
done
curl -fsS -o /dev/null "http://127.0.0.1:$APP_PORT/" || { echo "Бухгалтерия не поднялась, см. $WORK/app.log"; tail -20 "$WORK/app.log"; exit 1; }

# 3. Токен: в БД хранится только SHA-256 строки fbt_… (kernel::crypto::sha256_hex)
new_token() { # $1 — право; печатает токен
  local t h
  t="fbt_it$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  h=$(printf '%s' "$t" | sha256sum | cut -d' ' -f1)
  "${PSQL[@]}" -d family_budget >/dev/null <<SQL
INSERT INTO fb_api_tokens (id, user_id, project_id, name, token_hash, scopes)
SELECT gen_random_uuid(), u.id, p.id, 'integration $1', '$h', ARRAY['$1']
FROM fb_users u
JOIN fb_project_members m ON m.user_id = u.id
JOIN fb_projects p ON p.id = m.project_id AND p.project_kind = 'FAMILY'
WHERE u.email = 'aleksey@demo.local'
LIMIT 1;
SQL
  echo "$t"
}
TOKEN=$(new_token write)
TOKEN_READ=$(new_token read)
curl -fsS -o /dev/null -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$APP_PORT/api/accounts/CARD_KBANK" \
  || { echo "Токен не работает (демо-данные?), см. $WORK/app.log"; exit 1; }

cat >"$WORK/env.sh" <<ENV
export IT_API_URL=http://127.0.0.1:$APP_PORT
export IT_TOKEN=$TOKEN
export IT_TOKEN_READ=$TOKEN_READ
export IT_WORK=$WORK
ENV
echo "Бухгалтерия: http://127.0.0.1:$APP_PORT (вход для разработки — aleksey@demo.local), лог $WORK/app.log"
echo "Окружение для it.py: source $WORK/env.sh"
