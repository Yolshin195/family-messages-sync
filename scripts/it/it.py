#!/usr/bin/env python3
"""Интеграционная проверка family-messages-sync ↔ Семейная бухгалтерия.

Контракты — docs/mvp.md (приложение) и docs/external-refs.md (family-budget). Только стандартная библиотека.

    python3 scripts/it/it.py contract                 API бухгалтерии (CLI не нужен)
    python3 scripts/it/it.py e2e --cli <бинарник>     сквозные сценарии: поддельный chat.db → CLI → бухгалтерия
    python3 scripts/it/it.py all --cli <бинарник>     всё

Окружение: IT_API_URL, IT_TOKEN (токен с правом write в проекте с демо-данными: счета CARD_KBANK и CASH в THB,
категория FOOD_CAFE), IT_WORK (рабочий каталог). Их пишет scripts/it/budget-up.sh в $WORK/env.sh.
Бухгалтерия должна быть чистой (свежие демо-данные): суммы сценариев подобраны так, чтобы не совпасть с демо.
"""

import argparse
import concurrent.futures
import datetime as dt
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import traceback
import urllib.error
import urllib.parse
import urllib.request
import uuid

API = os.environ.get("IT_API_URL", "http://127.0.0.1:18080").rstrip("/")
TOKEN = os.environ.get("IT_TOKEN", "")
WORK = os.environ.get("IT_WORK", "/var/tmp/fms-it")
CARD, CASH = "CARD_KBANK", "CASH"
TODAY = dt.date.today()

# ---------------------------------------------------------------- HTTP


def http(method, path, body=None, query=None, token=None):
    url = API + path + ("?" + urllib.parse.urlencode(query) if query else "")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", "Bearer " + (token or TOKEN))
    req.add_header("Accept-Language", "ru")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw


def expense(amount, account=CARD, day=TODAY, category="FOOD_CAFE", kind="EXPENSE", **extra):
    body = {
        "occurred_on": day.isoformat(),
        "tx_kind": kind,
        "account_code": account,
        "items": [{"category_code": category, "amount": str(amount), "note": "заметка из API"}],
        "description": "проверка интеграции",
        "description_lang": "RU",
    }
    body.update(extra)
    return body


def transfer(amount, src=CARD, dst=CASH, day=TODAY):
    return {"occurred_on": day.isoformat(), "tx_kind": "TRANSFER", "account_code": src, "to_account_code": dst,
            "amount": str(amount), "description": "перевод для проверки", "description_lang": "RU"}


def create(body):
    st, b = http("POST", "/api/transactions", body)
    assert st in (200, 201), f"создание: {st} {b}"
    return b


def get_tx(tx_id):
    return http("GET", f"/api/transactions/{tx_id}")


def list_tx(account=CARD, days=3, **q):
    query = {"from": (TODAY - dt.timedelta(days=days)).isoformat(), "to": (TODAY + dt.timedelta(days=days)).isoformat(),
             "account_code": account, "size": "200", **q}
    st, b = http("GET", "/api/transactions", query=query)
    assert st == 200, f"список: {st} {b}"
    return b["content"]


def count_all():
    st, b = http("GET", "/api/transactions", query={"size": "1"})
    assert st == 200, f"список: {st} {b}"
    return b["page"]["totalElements"]


def ref(provider="it_test", external_id=None, received_at=None):
    r = {"provider": provider, "external_id": external_id or f"it-{uuid.uuid4()}"}
    if received_at:
        r["received_at"] = received_at
    return r


# ---------------------------------------------------------------- проверки

RESULTS = []


def check(name):
    def wrap(fn):
        fn.check_name = name
        return fn
    return wrap


def run_checks(fns):
    for fn in fns:
        try:
            fn()
            RESULTS.append((fn.check_name, True, ""))
            print(f"  ✅ {fn.check_name}")
        except AssertionError as e:
            RESULTS.append((fn.check_name, False, str(e)))
            print(f"  ❌ {fn.check_name}\n     {e}")
        except Exception as e:  # noqa: BLE001 — сбой проверки, а не провал контракта
            RESULTS.append((fn.check_name, False, f"{type(e).__name__}: {e}"))
            print(f"  💥 {fn.check_name}\n     {type(e).__name__}: {e}")
            traceback.print_exc(limit=2)


def eq(actual, expected, what):
    assert actual == expected, f"{what}: ожидалось {expected!r}, получено {actual!r}"


# ================================================================ 1. Контракты бухгалтерии (docs/external-refs.md)


@check("К1 POST с external_ref → 201, связь CREATED в external_refs")
def k1():
    r = ref(received_at="2026-09-21T10:51:07+07:00")
    st, b = http("POST", "/api/transactions", expense("201.01", external_ref=r))
    eq(st, 201, "статус")
    refs = b.get("external_refs")
    assert isinstance(refs, list) and len(refs) == 1, f"external_refs: {refs}"
    eq((refs[0]["provider"], refs[0]["external_id"], refs[0]["link"], refs[0]["transaction_id"]),
       (r["provider"], r["external_id"], "CREATED", b["id"]), "связь")
    assert refs[0].get("received_at"), "received_at пуст"
    assert refs[0].get("created_at"), "created_at пуст"


@check("К2 повтор того же external_ref (другая сумма) → 200, та же операция, без новой")
def k2():
    r = ref()
    _, first = http("POST", "/api/transactions", expense("202.02", external_ref=r))
    before = count_all()
    st, b = http("POST", "/api/transactions", expense("999.99", external_ref=r))
    eq(st, 200, "статус")
    eq(b["id"], first["id"], "id")
    eq(b["total"].rstrip("0").rstrip("."), "202.02", "сумма прежняя")
    eq(count_all(), before, "число операций")


@check("К3 тот же external_id у другого provider → новая операция")
def k3():
    eid = f"it-{uuid.uuid4()}"
    _, a = http("POST", "/api/transactions", expense("203.03", external_ref=ref("it_a", eid)))
    st, b = http("POST", "/api/transactions", expense("203.03", external_ref=ref("it_b", eid)))
    eq(st, 201, "статус")
    assert a["id"] != b["id"], "одна и та же операция"


@check("К4 GET /api/transactions?external_provider и GET /{id} отдают external_refs")
def k4():
    r = ref("it_filter")
    _, t = http("POST", "/api/transactions", expense("204.04", external_ref=r))
    _, plain = http("POST", "/api/transactions", expense("204.05"))
    ids = [x["id"] for x in list_tx(external_provider="it_filter")]
    assert t["id"] in ids and plain["id"] not in ids, f"фильтр: {ids}"
    st, one = get_tx(plain["id"])
    eq((st, one.get("external_refs")), (200, []), "у операции без связей — пустой список")
    _, one = get_tx(t["id"])
    eq(len(one["external_refs"]), 1, "связей у операции")


@check("К5 поиск связи: 200 + transaction_exists; неизвестная → 404 NOT_FOUND")
def k5():
    r = ref()
    _, t = http("POST", "/api/transactions", expense("205.05", external_ref=r))
    st, b = http("GET", "/api/transactions/external-refs", query={"provider": r["provider"], "external_id": r["external_id"]})
    eq(st, 200, "статус")
    eq((b["transaction_id"], b["transaction_exists"], b["link"]), (t["id"], True, "CREATED"), "связь")
    st, b = http("GET", "/api/transactions/external-refs", query={"provider": "it_test", "external_id": "нет-такой"})
    eq(st, 404, "неизвестная связь")
    eq(b.get("error") if isinstance(b, dict) else b, "NOT_FOUND", "код")


@check("К6 удалить операцию → повтор POST → 409 EXTERNAL_REF_DELETED, transaction_exists=false")
def k6():
    r = ref()
    _, t = http("POST", "/api/transactions", expense("206.06", external_ref=r))
    st, _ = http("DELETE", f"/api/transactions/{t['id']}")
    eq(st, 204, "удаление")
    st, b = http("POST", "/api/transactions", expense("206.06", external_ref=r))
    eq(st, 409, "статус")
    eq(b.get("error"), "EXTERNAL_REF_DELETED", "код")
    _, b = http("GET", "/api/transactions/external-refs", query={"provider": r["provider"], "external_id": r["external_id"]})
    eq(b.get("transaction_exists"), False, "transaction_exists")


@check("К7 прикрепление: 201 MATCHED, повтор 200, к другой операции 409 EXTERNAL_REF_TAKEN")
def k7():
    manual = create(expense("207.07"))
    other = create(expense("207.08"))
    r = ref()
    st, b = http("POST", f"/api/transactions/{manual['id']}/external-refs", r)
    eq(st, 201, "первое прикрепление")
    eq((b["link"], b["transaction_id"]), ("MATCHED", manual["id"]), "связь")
    st, _ = http("POST", f"/api/transactions/{manual['id']}/external-refs", r)
    eq(st, 200, "повтор к той же операции")
    st, b = http("POST", f"/api/transactions/{other['id']}/external-refs", r)
    eq(st, 409, "к другой операции")
    eq(b.get("error"), "EXTERNAL_REF_TAKEN", "код")
    _, one = get_tx(manual["id"])
    eq([x["link"] for x in one["external_refs"]], ["MATCHED"], "связи ручной операции")
    eq(one["total"].rstrip("0").rstrip("."), "207.07", "операция не изменилась")


@check("К8 прикрепление к несуществующей операции → 404")
def k8():
    st, _ = http("POST", f"/api/transactions/{uuid.uuid4()}/external-refs", ref())
    eq(st, 404, "статус")


@check("К9 две связи у перевода (SMS списания и зачисления)")
def k9():
    t = create(transfer("209.09"))
    for _ in range(2):
        st, _ = http("POST", f"/api/transactions/{t['id']}/external-refs", ref())
        eq(st, 201, "прикрепление")
    _, one = get_tx(t["id"])
    eq(len(one["external_refs"]), 2, "связей")


@check("К10 PUT правка не трогает связи и заметку позиции")
def k10():
    _, t = http("POST", "/api/transactions", expense("210.10", external_ref=ref()))
    item = t["items"][0]
    body = {"occurred_on": t["occurred_on"], "tx_kind": "EXPENSE", "account_code": CARD,
            "items": [{"id": item["id"], "category_code": "FOOD_GROCERIES"}], "description_lang": "RU"}
    st, b = http("PUT", f"/api/transactions/{t['id']}", body)
    if st == 422 and "FOOD_GROCERIES" in json.dumps(b, ensure_ascii=False):
        body["items"][0]["category_code"] = "TRANSPORT_TAXI"
        st, b = http("PUT", f"/api/transactions/{t['id']}", body)
    eq(st, 200, f"правка {b}")
    eq(len(b["external_refs"]), 1, "связи после правки")
    eq(b["items"][0].get("note"), "заметка из API", "заметка позиции")


@check("К11 проверка полей: provider, external_id, received_at → 422 с field")
def k11():
    for bad, field in [({"provider": "Bad-Prov!", "external_id": "x"}, "external_ref.provider"),
                       ({"provider": "it_test", "external_id": "   "}, "external_ref.external_id"),
                       ({"provider": "it_test", "external_id": "x" * 201}, "external_ref.external_id"),
                       ({"provider": "it_test", "external_id": "x", "received_at": "21.09.2026"}, "external_ref.received_at")]:
        st, b = http("POST", "/api/transactions", expense("211.11", external_ref=bad))
        eq(st, 422, f"статус для {bad}")
        eq(b.get("field"), field, f"field для {bad}")


@check("К12 provider приводится к нижнему регистру, external_id — trim")
def k12():
    eid = f"it-{uuid.uuid4()}"
    _, a = http("POST", "/api/transactions", expense("212.12", external_ref={"provider": " IT_Case ", "external_id": f"  {eid} "}))
    st, b = http("POST", "/api/transactions", expense("212.12", external_ref={"provider": "it_case", "external_id": eid}))
    eq(st, 200, "повтор в нормализованном виде")
    eq(b["id"], a["id"], "id")


@check("К13 начальный остаток: связь нельзя (422)")
def k13():
    st, b = http("GET", "/api/transactions", query={"tx_kind": "OPENING", "account_code": CARD, "size": "1"})
    assert st == 200 and b["content"], f"нет начального остатка в демо: {st}"
    st, _ = http("POST", f"/api/transactions/{b['content'][0]['id']}/external-refs", ref())
    eq(st, 422, "прикрепление к OPENING")


@check("К14 гонка: 8 одинаковых POST параллельно → одна операция, все получают её id")
def k14():
    r = ref()
    before = count_all()
    with concurrent.futures.ThreadPoolExecutor(8) as ex:
        res = list(ex.map(lambda _: http("POST", "/api/transactions", expense("214.14", external_ref=r)), range(8)))
    codes = sorted(st for st, _ in res)
    ids = {b["id"] for st, b in res if st in (200, 201)}
    eq(codes.count(201), 1, f"ровно один 201 (коды {codes})")
    eq(len(ids), 1, "один id у всех")
    eq(count_all(), before + 1, "операций добавилось")


@check("К15 права: токен только на чтение — 403 на создание и прикрепление, поиск разрешён; неизвестный токен — 401")
def k15():
    t = create(expense("215.15"))
    st, _ = http("POST", f"/api/transactions/{t['id']}/external-refs", ref(), token="fbt_unknown")
    eq(st, 401, "неизвестный токен")
    read = os.environ.get("IT_TOKEN_READ")
    assert read, "нет IT_TOKEN_READ (обновите budget-up.sh)"
    st, _ = http("POST", f"/api/transactions/{t['id']}/external-refs", ref(), token=read)
    eq(st, 403, "прикрепление токеном на чтение")
    st, _ = http("POST", "/api/transactions", expense("215.16", external_ref=ref()), token=read)
    eq(st, 403, "создание токеном на чтение")
    st, _ = http("GET", "/api/transactions/external-refs", query={"provider": "it_test", "external_id": "x"}, token=read)
    eq(st, 404, "поиск токеном на чтение")


def mcp(method, params, rid=1):
    req = urllib.request.Request(API + "/mcp", data=json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}).encode(),
                                 method="POST")
    for k, v in {"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json",
                 "Accept": "application/json, text/event-stream"}.items():
        req.add_header(k, v)
    with urllib.request.urlopen(req, timeout=20) as r:
        raw = r.read().decode()
    if raw.startswith("event:") or raw.startswith("data:"):
        raw = next(line[5:] for line in raw.splitlines() if line.startswith("data:"))
    return json.loads(raw)


@check("К16 MCP: инструкция про SMS, list_transactions(from_sms) отдаёт только операции из SMS с полем sms")
def k16():
    init = mcp("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "it", "version": "1"}})
    instr = init["result"].get("instructions", "")
    assert "from_sms" in instr and "update_transaction" in instr, "в INSTRUCTIONS нет абзаца про операции из SMS"
    _, t = http("POST", "/api/transactions", expense("216.16", external_ref=ref("imessage")))
    create(expense("216.17"))
    res = mcp("tools/call", {"name": "list_transactions", "arguments": {"from_sms": True, "account": CARD, "limit": 200}}, 2)
    text = res["result"]["content"][0]["text"]
    data = json.loads(text[text.index("{"):])
    txs = data["transactions"]
    assert any(x["id"] == t["id"] for x in txs), "операции из SMS нет в from_sms"
    assert all(x.get("sms") for x in txs), "в from_sms попала операция без поля sms"
    assert not any(x["total"].startswith("216.17") for x in txs), "в from_sms попала ручная операция"
    eq(next(x for x in txs if x["id"] == t["id"])["sms"]["link"], "CREATED", "sms.link")


CONTRACT = [k1, k2, k3, k4, k5, k6, k7, k8, k9, k10, k11, k12, k13, k14, k15, k16]

# ================================================================ 2. Сквозные сценарии (docs/mvp.md)

APPLE_EPOCH = dt.datetime(2001, 1, 1, tzinfo=dt.timezone.utc)


class Mac:
    """Один «Mac»: свой HOME (конфиг, state.json, app.db), общий поддельный chat.db."""

    def __init__(self, cli, root, name, chat):
        self.cli, self.chat = cli, chat
        self.home = os.path.join(root, name)
        self.cfg_dir = os.path.join(self.home, ".config", "family-messages-sync")
        os.makedirs(self.cfg_dir, exist_ok=True)
        self.token = TOKEN
        self.write_config()

    def write_config(self, **over):
        cfg = {"api_url": API, "default_account": CARD, "accounts": {"5019": CARD, "8842": CASH},
               "delay_minutes": 0, "date_tolerance_days": 1, "notify_created": True, "old_message_days": 2}
        cfg.update(over)
        with open(os.path.join(self.cfg_dir, "config.json"), "w") as f:
            json.dump(cfg, f, ensure_ascii=False, indent=2)

    def run(self, *args, token=None):
        env = dict(os.environ, HOME=self.home, FAMILY_SYNC_CHAT_DB=self.chat, FAMILY_SYNC_TOKEN=token or self.token)
        p = subprocess.run([self.cli, *args], env=env, capture_output=True, text=True, timeout=120)
        return p.returncode, p.stdout + p.stderr

    def ok(self, *args):
        code, out = self.run(*args)
        assert code == 0, f"`{' '.join(args)}` → код {code}:\n{out}"
        return out

    def journal(self, rowid):
        path = os.path.join(self.cfg_dir, "app.db")
        if not os.path.exists(path):
            return None
        with sqlite3.connect(path) as db:
            db.row_factory = sqlite3.Row
            r = db.execute("SELECT * FROM messages WHERE rowid = ?", (rowid,)).fetchone()
            return dict(r) if r else None

    def state(self):
        with open(os.path.join(self.cfg_dir, "state.json")) as f:
            return json.load(f)


class ChatDb:
    """Поддельный ~/Library/Messages/chat.db: только таблицы и колонки, которые читает приложение."""

    def __init__(self, path):
        self.path = path
        if os.path.exists(path):
            os.remove(path)
        with sqlite3.connect(path) as db:
            db.executescript("""
                CREATE TABLE handle (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL);
                CREATE TABLE message (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL, text TEXT,
                    attributedBody BLOB, date INTEGER, handle_id INTEGER DEFAULT 0, is_from_me INTEGER DEFAULT 0);
                INSERT INTO handle (id) VALUES ('KBank'), ('+66812345678');
            """)

    def add(self, text, sender="KBank", received=None):
        received = received or dt.datetime.now(dt.timezone.utc)
        ns = int((received - APPLE_EPOCH).total_seconds() * 1_000_000_000)
        guid = str(uuid.uuid4()).upper()
        with sqlite3.connect(self.path) as db:
            hid = db.execute("SELECT ROWID FROM handle WHERE id = ?", (sender,)).fetchone()[0]
            cur = db.execute("INSERT INTO message (guid, text, date, handle_id) VALUES (?, ?, ?, ?)", (guid, text, ns, hid))
            return cur.lastrowid, guid


def sms(amount, kind="Withdrawal", acc="5019", day=TODAY, tail="Outstanding Balance 8,195.25 Baht."):
    return f"{day:%d/%m/%y} 10:50 A/C X-{acc} {kind} {amount} {tail}"


def refs_for(guid):
    st, b = http("GET", "/api/transactions/external-refs", query={"provider": "imessage", "external_id": guid})
    return b if st == 200 else None


def e2e_checks(cli):
    root = os.path.join(WORK, "e2e")
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    chat = ChatDb(os.path.join(root, "chat.db"))
    mac = Mac(cli, root, "mac1", chat.path)
    ctx = {}

    @check("Э0 init проверяет связь и версию бухгалтерии, создаёт AUTO_*; первый run только запоминает позицию")
    def e0():
        mac.ok("init")
        ctx["first_rowid"], _ = chat.add(sms("1.00"))
        before = count_all()
        mac.ok("run")
        eq(count_all(), before, "первый запуск ничего не заводит")
        assert mac.state().get("last_rowid"), "last_rowid не записан"

    @check("Э1 С2: SMS без ручной записи → CREATED, связь imessage CREATED, AUTO_EXPENSE")
    def e1():
        rowid, guid = chat.add(sms("301.31"))
        mac.ok("run")
        j = mac.journal(rowid)
        assert j, "нет строки в app.db"
        eq(j["status"], "CREATED", "статус в журнале")
        r = refs_for(guid)
        assert r, "в бухгалтерии нет связи imessage для guid"
        eq((r["link"], r["transaction_id"], r["transaction_exists"]), ("CREATED", j["transaction_id"], True), "связь")
        _, t = get_tx(j["transaction_id"])
        eq((t["account_code"], t["occurred_on"], t["items"][0]["category_code"]), (CARD, TODAY.isoformat(), "AUTO_EXPENSE"), "операция")
        assert "[sms:" not in (t["items"][0].get("note") or ""), "в заметке осталась старая метка [sms:]"
        ctx["created"] = (rowid, guid, j["transaction_id"])

    @check("Э2 повторный run → ничего нового")
    def e2():
        before = count_all()
        mac.ok("run")
        eq(count_all(), before, "число операций")

    @check("Э3 С1: ручная запись до SMS → MATCHED, у ручной связь MATCHED, операция не создана")
    def e3():
        manual = create(expense("303.33"))
        before = count_all()
        rowid, guid = chat.add(sms("303.33"))
        mac.ok("run")
        eq(mac.journal(rowid)["status"], "MATCHED", "статус")
        eq(mac.journal(rowid)["transaction_id"], manual["id"], "transaction_id")
        eq(count_all(), before, "число операций")
        _, t = get_tx(manual["id"])
        eq([(x["provider"], x["link"]) for x in t["external_refs"]], [("imessage", "MATCHED")], "связи ручной")

    @check("Э4 С4: две одинаковые покупки, одна внесена → одна MATCHED, другая CREATED")
    def e4():
        create(expense("304.44"))
        r1, _ = chat.add(sms("304.44", tail="Outstanding Balance 7,000.00 Baht."))
        r2, _ = chat.add(sms("304.44", tail="Outstanding Balance 6,695.56 Baht."))
        mac.ok("run")
        eq(sorted([mac.journal(r1)["status"], mac.journal(r2)["status"]]), ["CREATED", "MATCHED"], "статусы")

    @check("Э5 ручная, уже подтверждённая другим SMS, не достаётся второму")
    def e5():
        manual = create(expense("305.55"))
        r1, _ = chat.add(sms("305.55"))
        mac.ok("run")
        eq(mac.journal(r1)["status"], "MATCHED", "первое SMS")
        r2, _ = chat.add(sms("305.55", tail="Outstanding Balance 5,000.00 Baht."))
        mac.ok("run")
        j = mac.journal(r2)
        eq(j["status"], "CREATED", "второе SMS")
        assert j["transaction_id"] != manual["id"], "второе SMS прикрепилось к уже занятой ручной"

    @check("Э6 С12: перевод внесён вручную, пришли SMS списания и зачисления → оба MATCHED к переводу")
    def e6():
        t = create(transfer("306.66"))
        r1, _ = chat.add(f"{TODAY:%d/%m/%y} 21:26 A/C X-5019 transferred 306.66 Baht to A/C X-8842 Outstanding Balance 1.00 Baht.")
        r2, _ = chat.add(sms("306.66", kind="Deposit", acc="8842"))
        mac.ok("run")
        eq((mac.journal(r1)["status"], mac.journal(r2)["status"]), ("MATCHED", "MATCHED"), "статусы")
        eq((mac.journal(r1)["transaction_id"], mac.journal(r2)["transaction_id"]), (t["id"], t["id"]), "обе к переводу")
        _, one = get_tx(t["id"])
        eq(len(one["external_refs"]), 2, "связей у перевода")

    @check("Э7 С8, С9: OTP → IGNORED без текста; неизвестный формат → UNRECOGNIZED; чужой отправитель — не в журнале")
    def e7():
        r1, _ = chat.add("OTP 482913 for Withdrawal 500.00 — do not share")
        r2, _ = chat.add("Your statement is ready. KBank")
        r3, _ = chat.add(sms("307.77"), sender="+66812345678")
        before = count_all()
        mac.ok("run")
        j1 = mac.journal(r1)
        eq((j1["status"], j1["text"]), ("IGNORED", ""), "OTP")
        eq(mac.journal(r2)["status"], "UNRECOGNIZED", "неизвестный")
        eq(mac.journal(r3), None, "не от банка")
        eq(count_all(), before, "операций")

    @check("Э8 dry-run: ничего не создаёт и не пишет в журнал; затем обычный run создаёт")
    def e8():
        rowid, _ = chat.add(sms("308.88"))
        before = count_all()
        out = mac.ok("run", "--dry-run")
        eq((count_all(), mac.journal(rowid)), (before, None), "после dry-run")
        assert "308.88" in out, f"dry-run не показал сообщение:\n{out}"
        mac.ok("run")
        eq(mac.journal(rowid)["status"], "CREATED", "после run")

    @check("Э9 П4: подкачка истории (новый ROWID, получено 10 дней назад) → OLD, без операции")
    def e9():
        old = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=10)
        rowid, guid = chat.add(sms("309.99", day=old.date()), received=old)
        before = count_all()
        mac.ok("run")
        eq(mac.journal(rowid)["status"], "OLD", "статус")
        eq((count_all(), refs_for(guid)), (before, None), "операция и связь")
        ctx["old_rowid"] = rowid

    @check("Э10 С10: бухгалтерия недоступна → код ≠ 0, last_error, сообщение не потеряно; после восстановления — CREATED")
    def e10():
        rowid, _ = chat.add(sms("310.10"))
        mac.write_config(api_url="http://127.0.0.1:9")
        code, out = mac.run("run")
        assert code != 0, f"ожидался ненулевой код:\n{out}"
        st = mac.state()
        assert st.get("last_error") and not st["last_error"].get("resolved"), f"last_error: {st.get('last_error')}"
        eq(mac.journal(rowid), None, "сообщение не обработано")
        mac.write_config()
        mac.ok("run")
        eq(mac.journal(rowid)["status"], "CREATED", "после восстановления")
        assert mac.state()["last_error"].get("resolved"), "last_error не помечен исправленным"

    @check("Э11 С10: токен отозван → 401, код ≠ 0; с верным токеном догоняет")
    def e11():
        rowid, _ = chat.add(sms("311.11"))
        code, out = mac.run("run", token="fbt_revoked")
        assert code != 0 and "401" in out, f"ожидалась остановка с 401:\n{out}"
        eq(mac.journal(rowid), None, "сообщение не обработано")
        mac.ok("run")
        eq(mac.journal(rowid)["status"], "CREATED", "после восстановления")

    @check("Э12 С6: операцию из SMS удалили → run --from-rowid → DELETED, не создаётся")
    def e12():
        rowid, guid = chat.add(sms("312.12"))
        mac.ok("run")
        tx_id = mac.journal(rowid)["transaction_id"]
        st, _ = http("DELETE", f"/api/transactions/{tx_id}")
        eq(st, 204, "удаление")
        before = count_all()
        mac.ok("run", "--from-rowid", str(rowid - 1))
        eq(mac.journal(rowid)["status"], "DELETED", "статус")
        eq(count_all(), before, "операций")

    @check("Э13 С5, С7: правка операции из SMS, затем вся история заново → новые только у ни разу не заведённых (первое SMS, OLD)")
    def e13():
        rowid, guid, tx_id = ctx["created"]
        _, t = get_tx(tx_id)
        body = {"occurred_on": t["occurred_on"], "tx_kind": "EXPENSE", "account_code": CARD,
                "items": [{"id": t["items"][0]["id"], "category_code": "FOOD_CAFE"}], "description": "кофе", "description_lang": "RU"}
        st, b = http("PUT", f"/api/transactions/{tx_id}", body)
        eq(st, 200, f"правка {b}")
        before = count_all()
        mac.ok("run", "--from-rowid", "0")
        eq(mac.journal(rowid)["status"], "ALREADY_SENT", "статус исправленной")
        # --from-rowid отсечку П4 не применяет, а первое SMS при первом запуске только запоминало позицию:
        # эти два заводятся впервые, всё остальное — уже отправлено
        eq((mac.journal(ctx["first_rowid"])["status"], mac.journal(ctx["old_rowid"])["status"]), ("CREATED", "CREATED"), "ни разу не заведённые")
        eq(count_all(), before + 2, "новых операций")

    @check("Э14 сброс состояния (state.json и app.db удалены) + --from-rowid 0 → ни одной новой операции")
    def e14():
        for f in ("state.json", "app.db"):
            p = os.path.join(mac.cfg_dir, f)
            if os.path.exists(p):
                os.remove(p)
        before = count_all()
        mac.ok("run", "--from-rowid", "0")
        eq(count_all(), before, "операций")

    @check("Э15 два Mac на одном Apple ID запускаются одновременно → одна операция")
    def e15():
        mac2 = Mac(cli, root, "mac2", chat.path)
        mac2.ok("run")  # первый запуск mac2 — запоминает позицию
        rowid, guid = chat.add(sms("315.15"))
        before = count_all()
        with concurrent.futures.ThreadPoolExecutor(2) as ex:
            res = list(ex.map(lambda m: m.run("run"), [mac, mac2]))
        assert all(c == 0 for c, _ in res), f"коды: {[c for c, _ in res]}\n{res}"
        eq(count_all(), before + 1, "операций добавилось")
        eq(sorted([mac.journal(rowid)["status"], mac2.journal(rowid)["status"]]), ["ALREADY_SENT", "CREATED"], "статусы")

    @check("Э16 команды log и status работают и показывают статусы")
    def e16():
        out = mac.ok("log", "50")
        assert "/transactions/" in out, f"в log нет ссылок на операции:\n{out}"
        mac.ok("log", "--status", "old")
        out = mac.ok("status")
        assert "Последний запуск" in out, f"status:\n{out}"

    run_checks([e0, e1, e2, e3, e4, e5, e6, e7, e8, e9, e10, e11, e12, e13, e14, e15, e16])


# ================================================================ main


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["contract", "e2e", "all"])
    ap.add_argument("--cli", help="путь к бинарнику family-messages-sync (для e2e и all)")
    a = ap.parse_args()
    if not TOKEN:
        sys.exit("Нет IT_TOKEN: запустите scripts/it/budget-up.sh и source $WORK/env.sh")
    if a.what in ("e2e", "all") and not (a.cli and os.access(a.cli, os.X_OK)):
        sys.exit("Нужен --cli <путь к собранному family-messages-sync>")
    print(f"Бухгалтерия: {API}")
    if a.what in ("contract", "all"):
        print("\nКонтракты бухгалтерии (docs/external-refs.md):")
        run_checks(CONTRACT)
    if a.what in ("e2e", "all"):
        print("\nСквозные сценарии (docs/mvp.md):")
        e2e_checks(os.path.abspath(a.cli))
    failed = [r for r in RESULTS if not r[1]]
    print(f"\nИтого: {len(RESULTS) - len(failed)} из {len(RESULTS)} пройдено")
    with open(os.path.join(WORK, "it-report.json"), "w") as f:
        json.dump([{"check": n, "ok": ok, "error": e} for n, ok, e in RESULTS], f, ensure_ascii=False, indent=2)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
