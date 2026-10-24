# Интеграционная проверка: приложение ↔ Семейная бухгалтерия

Проверяет контракты из `docs/mvp.md` (приложение) и `docs/external-refs.md` в family-budget на живой бухгалтерии
и настоящем бинарнике. macOS не нужна: `chat.db` подделывается (`FAMILY_SYNC_CHAT_DB`), токен берётся из
`FAMILY_SYNC_TOKEN`, а не из Keychain, у каждого «Mac» свой `HOME`.

| Файл | Что делает |
| --- | --- |
| `budget-up.sh` | Поднимает PostgreSQL и бухгалтерию с демо-данными (`APP_ENV=dev`), выпускает токены `write` и `read` проекта «Семья», пишет `$WORK/env.sh` |
| `it.py contract` | 16 проверок API бухгалтерии: создание с `external_ref`, повтор, удаление, прикрепление, поиск, фильтр, проверка полей, гонка, права, MCP |
| `it.py e2e --cli …` | 17 сквозных сценариев: SMS в поддельный `chat.db` → `family-messages-sync run` → журнал `app.db` и операции в бухгалтерии |
| `ui.mjs` | 6 проверок сайта в Chromium (Playwright): значок «SMS» в ленте и форме, правка формой не стирает связь и заметку |

## Запуск (Linux, облачная сессия Claude Code)

```bash
# 1. Бухгалтерия: checkout нужной ветки family-budget (соберётся, если нет target/debug/family_budget)
scripts/it/budget-up.sh ../family-budget          # REBUILD=1 — пересобрать
source /var/tmp/fms-it/env.sh

# 2. Приложение
cargo build --release

# 3. Проверки
python3 -I scripts/it/it.py all --cli target/release/family-messages-sync
NODE_PATH=$(npm root -g) node scripts/it/ui.mjs

scripts/it/budget-up.sh --down                    # остановить
```

- `budget-up.sh` каждый раз пересоздаёт базу: e2e рассчитан на свежие демо-данные (суммы сценариев 2xx.xx–4xx.xx
  не совпадают с демо). Повторный `it.py` без пересоздания: контракты проходят, e2e — тоже (каждый прогон создаёт
  свой `chat.db`), но в бухгалтерии копятся операции.
- Итог — код возврата (0 — всё прошло) и `$WORK/it-report.json`; скриншоты сайта — `$WORK/ui-*.png`; лог бухгалтерии —
  `$WORK/app.log`; журналы «Mac» — `$WORK/e2e/mac1/.config/family-messages-sync/`.
- Переменные: `WORK` (`/var/tmp/fms-it`), `PG_BIN` (`/usr/lib/postgresql/16/bin`), `PG_PORT` (55432), `APP_PORT` (18080).

## На Mac

`budget-up.sh` рассчитан на Linux. На Mac: `make dev` в family-budget (PostgreSQL и Rauthy в Podman), токен с правом
`write` в «Профиль → Безопасность» проекта «Семья» демо-пользователя, затем:

```bash
export IT_API_URL=http://localhost:8080 IT_TOKEN=fbt_… IT_TOKEN_READ=fbt_… IT_WORK=/tmp/fms-it
mkdir -p $IT_WORK && python3 -I scripts/it/it.py all --cli target/release/family-messages-sync
```

На Mac уведомления в e2e появятся по-настоящему (их будет несколько десятков) — это нормально.

## Сопоставление со сценариями ТЗ

| ТЗ (`docs/mvp.md`, 12) | Проверка |
| --- | --- |
| С1 ручная до SMS | Э3 |
| С2 SMS без ручной | Э1, `ui.mjs` С1–С2 |
| С4 две одинаковые | Э4, Э5 |
| С5 правка на сайте, затем `--from-rowid` | Э13, `ui.mjs` С4–С6, К10 |
| С6 удалена | Э12, К6 |
| С7 сброс состояния | Э14 |
| С8 OTP, С9 неизвестный формат | Э7 |
| С10 нет сети, токен отозван | Э10, Э11 |
| С12 перевод, два SMS | Э6, К9 |
| П4 подкачка истории | Э9, Э13 |
| Два Mac | Э15, К14 |
| `log`, `status`, `init` | Э0, Э16 |
| С3 Claude дополняет операцию из SMS | К16 (инструкция и `from_sms`); поведение Claude — вручную, ниже |

## Только вручную на Mac

Автоматически не проверить — пройти один раз перед пробным запуском:

1. **Настоящий `chat.db`.** `family-messages-sync show 20`: тексты SMS KBank читаются (в том числе из
   `attributedBody`), суммы, счета и даты разобраны верно.
2. **Full Disk Access и launchd.** `make fda`, `make agent`, `make status` — `last exit code = 0`; через 5 минут
   `family-messages-sync status` показывает свежий «Последний запуск».
3. **Keychain.** Запуск из launchd не спрашивает доступ (или после «Always Allow» больше не спрашивает).
4. **Уведомления.** Отключить Wi-Fi → `make run-now` → уведомление «Импорт SMS остановлен»; включить → «снова
   работает». Настоящая покупка без записи → «Из SMS: расход …».
5. **Claude (С3).** Дождаться SMS о покупке, затем написать Claude «кофе N бат»: он должен дополнить операцию из SMS
   («дополнил операцию из SMS …»), а не создать вторую.
6. **Сон Mac (С11).** Закрыть крышку на ночь — утром SMS за ночь заведены с датами из текста.
