# ТЗ: приложение для пробного запуска

Цель — запустить `family-messages-sync` на Mac владельца в фоне и пару недель проверить на настоящих SMS KBank.
Сейчас приложение помечает свои операции текстом `[sms:guid]` в заметке позиции и помнит «занятые» ручные
операции только локально. Переходим на служебные связи бухгалтерии и добавляем то, без чего в фоне работать нельзя:
журнал сообщений, уведомления, защиту от подкачки истории и команду состояния.

**Зависимость.** Нужна бухгалтерия со связями операций — ТЗ `docs/external-refs.md` в репозитории family-budget.
Контракты API ниже взяты оттуда; при расхождении прав тот документ.

Большое ТЗ (`docs/tz.md`) — следующий этап; журнал `app.db` здесь — подмножество его таблицы `messages` (11.1).

| # | Доработка | Раздел |
| --- | --- | --- |
| П1 | Связи бухгалтерии вместо метки и локального `claimed` | 3 |
| П2 | Журнал сообщений `app.db`, команда `log` | 4 |
| П3 | Уведомления macOS | 5 |
| П4 | Старые сообщения (подкачка истории) не заводятся | 6 |
| П5 | Команда `status` | 7 |

## 1. Файлы и данные

Каталог — `~/.config/family-messages-sync/`.

| Файл | Было | Стало |
| --- | --- | --- |
| `config.json` | настройки | + `provider`, `notify_created`, `old_message_days` (1.1) |
| `state.json` | `last_rowid`, `claimed`, `failed` | `claimed` удалён; + `last_received_at`, `last_run_at`, `last_ok_at`, `last_error`, `alert` (1.2) |
| `app.db` | — | новый: журнал сообщений (1.3) |
| `unparsed.log`, `failed.log` | как есть | как есть |

### 1.1. `config.json` — новые поля

Все с `#[serde(default)]`: старый конфиг читается без изменений.

| Поле | Тип | По умолчанию | Что |
| --- | --- | --- | --- |
| `provider` | string | `"imessage"` | `provider` связей в бухгалтерии. Не менять без причины |
| `notify_created` | bool | `true` | Уведомление на каждую созданную операцию |
| `old_message_days` | int | `2` | Отсечка П4, дней; `0` — выключить |

Существующие поля не меняются. `skip_if_already_entered`, `date_tolerance_days`, `delay_minutes`, `review_marker`,
категории `AUTO_*` — работают как сейчас.

### 1.2. `state.json`

```json
{
  "last_rowid": 48220,
  "failed": [48190],
  "last_received_at": "2026-09-21T13:02:11+07:00",
  "last_run_at": "2026-10-24T14:05:00+07:00",
  "last_ok_at": "2026-10-24T14:05:00+07:00",
  "last_error": { "text": "401 токен недействителен…", "at": "2026-10-23T09:10:00+07:00", "resolved": true },
  "alert": { "reason": "401", "at": "2026-10-23T09:10:00+07:00", "stopped": false }
}
```

```rust
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub last_rowid: Option<i64>,
    #[serde(default)] pub failed: Vec<i64>,
    /// Время получения последнего обработанного сообщения от банка (П4)
    #[serde(default)] pub last_received_at: Option<DateTime<Local>>,
    #[serde(default)] pub last_run_at: Option<DateTime<Local>>,
    /// Последний запуск без остановки
    #[serde(default)] pub last_ok_at: Option<DateTime<Local>>,
    #[serde(default)] pub last_error: Option<LastError>,   // { text, at, resolved }
    #[serde(default)] pub alert: Option<Alert>,            // { reason, at, stopped }
}
```

Старое поле `claimed` при чтении игнорируется (serde пропускает неизвестные поля) и при записи пропадает.

### 1.3. `app.db` — журнал сообщений

SQLite (`rusqlite`, уже подключён), открывается на чтение и запись в начале `run`, `log`, `show`, `status`.
Схема создаётся `CREATE TABLE IF NOT EXISTS`; версия — `PRAGMA user_version = 1`.

```sql
CREATE TABLE IF NOT EXISTS messages (
    rowid          INTEGER PRIMARY KEY,   -- ROWID из chat.db
    guid           TEXT    NOT NULL,
    sender         TEXT    NOT NULL,      -- handle.id
    received_at    TEXT    NOT NULL,      -- RFC 3339, местное время Mac
    text           TEXT    NOT NULL,      -- текст SMS; '' у IGNORED (коды OTP не храним)
    status         TEXT    NOT NULL CHECK (status IN
                     ('CREATED','MATCHED','ALREADY_SENT','DELETED','REJECTED','UNRECOGNIZED','IGNORED','OLD')),
    reason         TEXT    NULL,          -- текст отказа или пропуска
    kind           TEXT    NULL,          -- EXPENSE | INCOME
    amount         TEXT    NULL,          -- десятичная строка, как в SMS: '60.00'
    account_code   TEXT    NULL,
    occurred_on    TEXT    NULL,          -- YYYY-MM-DD
    transaction_id TEXT    NULL,          -- UUID операции в бухгалтерии
    processed_at   TEXT    NOT NULL       -- RFC 3339
);
CREATE INDEX IF NOT EXISTS messages_status ON messages (status, received_at);
CREATE INDEX IF NOT EXISTS messages_guid ON messages (guid);
```

| Статус | Когда | `transaction_id` |
| --- | --- | --- |
| `CREATED` | создана новая операция (`201`) | созданная |
| `MATCHED` | SMS прикреплено к уже внесённой операции | ручная |
| `ALREADY_SENT` | операция по этому SMS уже была (повтор, второй Mac, сброс состояния) | прежняя |
| `DELETED` | операцию по этому SMS удалили в бухгалтерии — не заводится | удалённая (если известна) |
| `REJECTED` | бухгалтерия отказала или не определён счёт; сообщение в очереди повторов | — |
| `UNRECOGNIZED` | от банка, но ни одно правило не подошло | — |
| `IGNORED` | совпало с `ignore_patterns` (OTP) | — |
| `OLD` | отсечено П4 | — |

Правила записи:

- `INSERT … ON CONFLICT(rowid) DO UPDATE SET …` — после обработки каждого сообщения, **до** сохранения
  `state.json`. Сбой между ними безопасен: следующий запуск обработает сообщение снова и обновит строку.
- Сообщения не от банка не пишутся. «Свежее, ждёт» (`delay_minutes`) не пишется.
- `REJECTED` при успешном повторе из очереди перезаписывается итоговым статусом.
- `--dry-run` в журнал не пишет.
- Чистка — в начале `run`: `DELETE FROM messages WHERE received_at < <now − 90 дней>`.

## 2. Обращения к бухгалтерии

Все запросы — `Authorization: Bearer fbt_…`, токен с правом `transactions:write`. Формат ошибок:
`{"error": "<КОД>", "message": "…", "field": "…"}`. Классификация общих ответов — как сейчас (`api.rs`
`access_error`): `401`, `403`, `429`, `5xx`, сеть, таймаут → **остановка** (`Stop`), повтор в следующий запуск.

| # | Запрос | Когда |
| --- | --- | --- |
| A1 | `GET /api/transactions?from&to&account_code&page&size=200` | кандидаты на пару и проверка «уже отправлено» |
| A2 | `POST /api/transactions` с `external_ref` | создать операцию |
| A3 | `POST /api/transactions/{id}/external-refs` | прикрепить SMS к ручной операции |
| A4 | `GET /api/transactions/external-refs?provider&external_id` | после `409` узнать операцию |
| A5 | `GET /api/accounts/{code}`, `GET/POST /api/categories` | `init`, как сейчас |

### 2.1. A1 — что читаем из операции

К существующим полям (`id`, `tx_kind`, `account_code`, `to_account_code`, `total`, `to_amount`, `fee_amount`,
`refund`, `occurred_on`, `items[].note`, `description`) добавляется:

```json
"external_refs": [
  { "provider": "imessage", "external_id": "p:0/4F1C…", "transaction_id": "7b1e…", "link": "CREATED",
    "received_at": "2026-09-21T03:51:07Z", "created_at": "2026-09-21T04:02:10Z" }
]
```

```rust
pub struct Existing {
    // …поля как сейчас, кроме sms_guid
    /// guid SMS, связанные с операцией: external_refs с provider == cfg.provider,
    /// плюс старая метка [sms:…] из заметок и описания (операции до перехода)
    pub sms_guids: Vec<String>,
}
```

Если в ответе нет поля `external_refs` — бухгалтерия старая: остановка с текстом «Обновите Семейную бухгалтерию:
нужна версия со связями SMS (docs/external-refs.md)».

### 2.2. A2 — создание

Тело — как сейчас (`build_payload`), плюс:

```json
"external_ref": { "provider": "imessage", "external_id": "<guid>", "received_at": "<msg.date RFC 3339>" }
```

Изменения тела: `items[0].note` — `"<review_marker> SMS: <текст до 300 символов>"`, **без** `[sms:guid]`.

| Ответ | Итог |
| --- | --- |
| `201` | `CREATED`, `transaction_id = body.id` |
| `200` | `ALREADY_SENT`, `transaction_id = body.id` (прошлый запуск создал, ответ не дошёл) |
| `409 EXTERNAL_REF_DELETED` | `DELETED`; `transaction_id` — из A4, если удалось; не в очередь повторов, без уведомления |
| другой `4xx` (кроме `401/403/408/429`) | `REJECTED`, текст ответа в `reason`, в очередь повторов — как сейчас |
| `401/403/408/429/5xx`, сеть | остановка |

### 2.3. A3 — прикрепление

Тело: `{ "provider": "imessage", "external_id": "<guid>", "received_at": "<RFC 3339>" }`.

| Ответ | Итог |
| --- | --- |
| `201`, `200` | `MATCHED`, `transaction_id = id` кандидата |
| `409 EXTERNAL_REF_TAKEN` | SMS уже прикреплено к другой операции → A4 → `ALREADY_SENT` с её id (или `DELETED`, если `transaction_exists = false`) |
| `404` | кандидата удалили между A1 и A3 → следующий кандидат, иначе A2 |
| прочее | как в 2.2 |

### 2.4. A4 — поиск связи

`200` → `{…ExternalRefDto, "transaction_exists": bool}`; `404` — связи нет (после `409` не ожидается: остановка с
текстом ответа).

## 3. П1. Алгоритм обработки сообщения

Изменения в `sync.rs` / `dedupe.rs`; порядок шагов до разбора — как сейчас.

1. Не от банка → пропустить. Совпало с `ignore_patterns` → `IGNORED`.
2. Не разобралось → `UNRECOGNIZED` (+ `unparsed.log`, как сейчас).
3. Свежее (`delay_minutes`) → ждать, остановить проход (как сейчас), в журнал не писать.
4. П4: старое → `OLD`.
5. Счёт не определён → `REJECTED` (как сейчас).
6. A1 по счёту за дату ±`date_tolerance_days`.
7. Есть операция с этим guid в `sms_guids` → `ALREADY_SENT`.
8. Если `skip_if_already_entered`: кандидаты — операции **без** guid в `sms_guids` (ни одно SMS их ещё не
   подтвердило), совпавшие по правилам `Existing::matches` (счёт, направление с учётом возврата, сумма, переводы)
   и дате ±допуск; порядок — ближайшая дата, затем `id`. Перевод — исключение: его могут подтвердить два SMS
   (списание и зачисление), поэтому у `TRANSFER` проверяется только, что нет SMS той же стороны (того же
   направления). По кандидатам по очереди — A3 (2.3).
9. Пары нет → A2 (2.2).
10. Записать строку журнала, затем `state.json` (`last_rowid`, `last_received_at`, очередь повторов).

`--dry-run`: шаги 1–8 без A2 и A3; печать «будет создано» / «будет прикреплено к <id>» / «уже отправлено».

Удаляется: `State.claimed`, `Claim`, параметр `claimed` в `judge`, создание `[sms:guid]` (`sms_marker` остаётся
только для чтения старых меток).

Уникальность на стороне сервера страхует от гонок и двух Mac: одно SMS — одна связь. Два SMS одновременно с двух Mac
к одной ручной операции — допустимый редкий случай (одна операция с двумя связями), исправляется вручную.

## 4. П2. Команда `log`

```
family-messages-sync log [N] [--status STATUS]
```

- `N` — сколько последних записей (по умолчанию 20), по `received_at` по убыванию.
- `--status` — `created | matched | already_sent | deleted | rejected | unrecognized | ignored | old`.

Формат строки:

```
#48211  21.09 10:51  расход   60.00 CARD_KBANK 21.09  создано          https://budget.example/transactions/7b1e…/edit
#48215  21.09 12:40  расход   89.00 CARD_KBANK 21.09  найдена ручная   https://budget.example/transactions/a3f0…/edit
#48219  21.09 12:58  доход   500.00 CARD_KBANK 21.09  отклонено        422 категория AUTO_INCOME не найдена
#48220  21.09 13:02  —                                не распознано    «Your statement is ready…»
```

- Ссылка — `{api_url}/transactions/{transaction_id}/edit`, если `transaction_id` есть.
- Последняя колонка у записей без операции — `reason` или начало текста (40 символов) у `UNRECOGNIZED`.
- Подписи статусов: создано, найдена ручная, уже было, удалена в бухгалтерии, отклонено, не распознано, пропущено
  (код), старое.

`show [N]` — как сейчас, плюс строка `  журнал: <статус> <ссылка>`, если сообщение есть в `app.db`.

## 5. П3. Уведомления macOS

Трейт `Notifier { fn notify(&self, title: &str, text: &str); }`. Реализация —
`osascript -e 'display notification "<text>" with title "<title>"'` (кавычки и `\` в тексте экранировать); ошибка
запуска `osascript` пишется в `err.log` и обработку не прерывает. В тестах — реализация, складывающая в `Vec`.
Заголовок — «Семейная бухгалтерия».

| Событие | Текст | Частота |
| --- | --- | --- |
| Остановка (`Stop`) или ошибка до цикла (нет доступа к `chat.db`, нет токена) | «Импорт SMS остановлен: <причина>. Подробности: make logs» | `alert.reason` — первые 40 символов причины; та же причина — не чаще раза в 6 часов |
| Новое сообщение в очереди повторов | «SMS не заведено: <reason коротко>. См. failed.log» | один раз на ROWID (когда впервые попадает в `failed`) |
| `CREATED` | «Из SMS: расход 60.00 THB, CARD_KBANK — проверьте категорию» | каждый раз, если `notify_created` |
| Запуск без остановки после остановки (`alert.stopped = true`) | «Импорт SMS снова работает» | один раз; `alert.stopped = false` |
| Больше 20 `OLD` за запуск | «Пропущено N старых SMS — похоже на подкачку истории. Список: family-messages-sync log --status old» | раз за запуск |

`--dry-run` уведомлений не показывает.

Ошибки, случающиеся до цикла (`open_db`, `keychain::get`, `Api::new`), обрабатываются в `cmd_run`: уведомление,
`last_error`, запись `state.json`, затем выход с ошибкой — как сейчас (код возврата `1`).

## 6. П4. Старые сообщения

- После каждого сообщения от банка (любой статус, кроме «ждёт») — `last_received_at = max(last_received_at,
  msg.date)`.
- Сообщение не из очереди повторов, у которого `msg.date < last_received_at − old_message_days` → `OLD`,
  `reason = "старое сообщение (подкачка истории?)"`, операция не заводится, в очередь повторов не попадает.
- Не применяется: при `old_message_days = 0`, при `run --from-rowid N`, когда `last_received_at` пусто.
- Найти и завести отсечённые: `log --status old`, затем `run --from-rowid <ROWID>`.

Зачем: подкачка истории iCloud даёт старым SMS новые ROWID, и без отсечки они завелись бы как новые. SMS за неделю
выключенного Mac новее последнего обработанного и заводятся.

## 7. П5. Команда `status`

```
family-messages-sync status
```

```
Последний запуск:  24.10 14:05 — ок
Последняя ошибка:  23.10 09:10 — 401 токен недействителен (исправлено)
Последнее SMS:     #48220, получено 21.09 13:02
Очередь повторов:  1 (#48219)
За 7 дней:         создано 12 · найдено ручных 30 · не распознано 1 · старых 0
```

- Первые три строки — из `state.json`; «исправлено» — `last_error.resolved`.
- Счётчики — `SELECT status, count(*) FROM messages WHERE received_at >= <now − 7 дней> GROUP BY status`.
- Не обращается к бухгалтерии и `chat.db`: работает и без Full Disk Access у терминала.
- `make status` после строк `launchctl` вызывает `$(BIN) status`.

`run` в конце (в т. ч. при ошибке) пишет `last_run_at`; без остановки — `last_ok_at`, `last_error.resolved = true`;
при остановке — `last_error = {text, at, resolved: false}`.

## 8. `init`

К текущим проверкам добавить проверку версии бухгалтерии: A4 с `provider=<cfg.provider>&external_id=probe`.

- `404` с телом `{"error": "NOT_FOUND", …}` или `200` — бухгалтерия знает связи, ок.
- Иначе (у старой версии путь попадает в `/{id}` и не разбирается как UUID — `400`) → ошибка «Обновите Семейную
  бухгалтерию: нужна версия со связями SMS (docs/external-refs.md)».

`run` дополнительно останавливается, если в непустом ответе A1 у операций нет поля `external_refs` (2.1).

## 9. Тесты

| # | Что | Ожидается |
| --- | --- | --- |
| U1 | `Existing::from_json` с `external_refs` и со старой меткой в заметке | оба guid в `sms_guids`; другой `provider` не попадает |
| U2 | `judge`: guid в `sms_guids` | уже отправлено |
| U3 | `judge`: ручная операция со связью другого SMS | не кандидат; без связи — кандидат |
| U4 | `judge`: перевод со связью SMS списания и SMS зачисления | зачисление — кандидат, второе списание — нет |
| U5 | Разбор ответов A2 (`201`, `200`, `409 EXTERNAL_REF_DELETED`, `422`, `500`) | статусы по 2.2 |
| U6 | Разбор ответов A3 (`201`, `200`, `409 EXTERNAL_REF_TAKEN`, `404`) | статусы по 2.3 |
| U7 | `build_payload` | есть `external_ref`; в `note` нет `[sms:` |
| U8 | Журнал (SQLite в памяти): каждый статус, `REJECTED` → `CREATED`, повторная запись одного ROWID, чистка 90 дней | одна строка на ROWID, верный статус |
| U9 | Отсечка П4 | старое → `OLD`; из очереди повторов, при `--from-rowid`, при `0` — не отсекается |
| U10 | Уведомления: одна причина дважды за 6 ч; разные причины; восстановление | 1; 2; одно «снова работает» |
| U11 | `state.json` со старым `claimed` | читается, при записи `claimed` нет |

HTTP-ответы в U5, U6 — через трейт над `Api` или локальный сервер на `std::net::TcpListener`; без сети.
`cargo test`, `cargo clippy -- -D warnings` — без ошибок.

## 10. README

- «Защита от дублей»: связи бухгалтерии вместо метки и «занятости»; ограничение «вручную после автоматической —
  два экземпляра» заменить: Claude сам дополняет операцию из SMS, на сайте — правьте операцию со значком SMS.
- Команды `log`, `status`; поля `notify_created`, `old_message_days`, `provider`; `app.db` в «Где что лежит».
- «Как проверить, что сообщение заведено»: `log`, значок «SMS» на сайте.
- Требование: версия бухгалтерии со связями SMS.

## 11. Порядок запуска

1. Бухгалтерия со связями задеплоена (`docs/external-refs.md` в family-budget).
2. `make install`, `family-messages-sync init` (проверит версию бухгалтерии).
3. Сухой прогон: `show 50` → ROWID SMS двухнедельной давности → `run --dry-run --from-rowid N`. Почти всё — «будет
   прикреплено» к ручным; «будет создано» — разобрать: невнесённые покупки или расхождение суммы, счёта, даты.
4. То же без `--dry-run`: ручные операции за две недели получают значок «SMS», невнесённые создаются.
5. `make fda`, `make agent`. Две недели: ежедневно `status`, раз в неделю `log 100` и разбор `[АВТО: проверить]`.

## 12. Сценарии приёмки на Mac

| # | Сценарий | Ожидается |
| --- | --- | --- |
| С1 | Покупка внесена через Claude до SMS | Операция не создаётся; у ручной — значок «SMS»; `log` — «найдена ручная» |
| С2 | Покупка не внесена | Через ~10–15 мин операция `AUTO_EXPENSE`, значок «SMS»; уведомление |
| С3 | SMS заведено, потом «кофе 60 бат» в Claude | Claude дополняет операцию из SMS; второй нет |
| С4 | Две одинаковые покупки за день, одна внесена | Одна — «найдена ручная», вторая — «создано» |
| С5 | Операция из SMS исправлена на сайте, затем `run --from-rowid` | «уже было», без дубля |
| С6 | Операция из SMS удалена, затем `run --from-rowid` | Не создаётся; `log` — «удалена в бухгалтерии» |
| С7 | `reset` и `run --from-rowid` за неделю | Ни одной новой операции |
| С8 | OTP | `IGNORED`, без текста |
| С9 | Неизвестный формат | `UNRECOGNIZED`, `unparsed.log` |
| С10 | Нет сети / токен отозван / нет Full Disk Access | Уведомление; после исправления догоняет, «снова работает» |
| С11 | Mac спал ночь | Утром SMS за ночь заведены с датами из текста |
| С12 | Перевод на свой счёт внесён вручную, пришли два SMS | Оба прикреплены к переводу |

**Готово, когда:** тесты U1–U11 зелёные, README обновлён, С1–С12 пройдены; за две недели — ни одного дубля и ни
одной молчаливой остановки.

## 13. Вне рамок

Флаг «не проверено» вместо категорий `AUTO_*`; «Входящие» и локальный веб-интерфейс; цепочка остатков и сверка;
пакетная отправка и право `autoimport:write`; подпись бинарника; ротация логов — по `docs/tz.md`.
