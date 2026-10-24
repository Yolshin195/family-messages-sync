//! Обработка новых сообщений: разбор, поиск в бухгалтерии, создание операций и связей SMS;
//! команды `show`, `log` и `status`.

use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate, SecondsFormat, SubsecRound};
use serde_json::{Value, json};
use std::{cell::RefCell, fs, path::Path};

use crate::{
    api::{Api, Ledger, Reply},
    config::{Config, LastError, NAME, State, app_dir, load_state, save_state},
    dedupe::{Probe, Verdict, judge},
    journal::{Entry, Journal, Status},
    logs,
    messages::{Msg, fetch_messages, max_rowid, open_db},
    notify::{self, MacNotifier, Notifier, TITLE, short},
    parse::{Kind, Matcher, Parsed},
};

/// Сколько дней очередь повторов держит отклонённое сообщение
const RETRY_DAYS: i64 = 14;
/// Сколько дней журнал помнит сообщение
const JOURNAL_DAYS: i64 = 90;
/// Больше стольких старых сообщений за запуск — уведомление о подкачке истории
const OLD_ALERT: usize = 20;

fn now() -> DateTime<Local> {
    Local::now().trunc_subsecs(0)
}

/// Текст описания по шаблону из конфига.
pub fn describe(cfg: &Config, p: &Parsed, account: &str, date: NaiveDate) -> String {
    let text = cfg
        .description_template
        .replace("{marker}", &cfg.review_marker)
        .replace("{kind}", p.kind.word())
        .replace("{amount}", &p.amount.to_string())
        .replace("{currency}", &cfg.currency)
        .replace("{merchant}", &p.merchant)
        .replace("{account}", account)
        .replace("{date}", &date.to_string());
    // пустые подстановки не должны оставлять двойные пробелы
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Связь SMS с операцией: по ней бухгалтерия узнаёт уже отправленное сообщение.
pub fn external_ref(cfg: &Config, msg: &Msg) -> Value {
    json!({
        "provider": cfg.provider,
        "external_id": msg.guid,
        "received_at": msg.date.to_rfc3339_opts(SecondsFormat::Secs, false),
    })
}

/// Тело `POST /api/transactions`.
pub fn build_payload(cfg: &Config, p: &Parsed, msg: &Msg, account: &str, date: NaiveDate) -> Value {
    let category = match p.kind {
        Kind::Expense => &cfg.expense_category,
        Kind::Income => &cfg.income_category,
    };
    let sms: String = msg.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
    let mut v = json!({
        "occurred_on": date.to_string(),
        "tx_kind": p.kind.tx_kind(),
        "account_code": account,
        "items": [{
            "category_code": category.code.trim().to_uppercase(),
            "amount": p.amount.to_string(),
            // чтобы при проверке видеть исходный SMS
            "note": format!("{} SMS: {}", cfg.review_marker, sms),
        }],
        "description": describe(cfg, p, account, date),
        "description_lang": cfg.description_lang.to_uppercase(),
        "external_ref": external_ref(cfg, msg),
    });
    if let Some(place) = cfg.place_code.as_deref().filter(|s| !s.trim().is_empty()) {
        v["place_code"] = json!(place.trim().to_uppercase());
    }
    if let Some(person) = cfg.for_person_code.as_deref().filter(|s| !s.trim().is_empty()) {
        v["for_person_code"] = json!(person.trim().to_uppercase());
    }
    v
}

/// Дата операции: из текста SMS, иначе дата получения; не из будущего.
fn operation_date(m: &Matcher, msg: &Msg) -> NaiveDate {
    let received = msg.date.date_naive();
    m.text_date(&msg.text).filter(|d| *d <= received + Duration::days(1)).unwrap_or(received)
}

fn log_line(dir: &Path, file: &str, line: &str) -> Result<()> {
    Ok(logs::append(&dir.join(file), line)?)
}

/// Последняя колонка журнала: ссылка на операцию, иначе причина, у нераспознанных — начало текста.
fn detail(e: &Entry, api_url: &str) -> String {
    match &e.transaction_id {
        Some(id) => format!("{}/transactions/{id}/edit", api_url.trim_end_matches('/')),
        None if e.status == Status::Unrecognized => format!("«{}»", short(&e.text, 40)),
        None => e.reason.clone().unwrap_or_default(),
    }
}

/// Строка журнала в `log` и выводе `run`:
/// `#48211  21.09 10:51  расход   60.00 CARD_KBANK 21.09  создано          https://…/transactions/7b1e…/edit`
fn entry_line(e: &Entry, label: &str, api_url: &str) -> String {
    let what = match (e.kind, &e.amount) {
        (Some(k), Some(amount)) => format!(
            "{:<6} {amount:>7} {} {}",
            k.word(),
            e.account_code.as_deref().unwrap_or("—"),
            e.occurred_on.map(|d| d.format("%d.%m").to_string()).unwrap_or_default()
        ),
        _ => "—".into(),
    };
    let rowid = format!("#{}", e.rowid);
    let line =
        format!("{rowid:<7} {}  {what:<31}  {label:<16} {}", e.received_at.format("%d.%m %H:%M"), detail(e, api_url));
    line.trim_end().to_string()
}

pub fn cmd_show(cfg: &Config, m: &Matcher, n: usize) -> Result<()> {
    let db = open_db()?;
    let journal = Journal::open(&app_dir())?;
    let banks: Vec<Msg> = fetch_messages(&db, 0, &[], &cfg.senders)?.into_iter().filter(|m| m.bank).collect();
    if banks.is_empty() {
        println!("Сообщений от банка не найдено — проверьте `senders` в конфиге.");
    }
    for msg in &banks[banks.len().saturating_sub(n)..] {
        println!("#{} {} [{}]", msg.rowid, msg.date.format("%Y-%m-%d %H:%M"), msg.handle);
        println!("  {}", msg.text);
        match m.parse(&msg.text) {
            None => println!("  -> не распознано"),
            Some(p) => {
                let account = cfg.resolve_account(p.account_hint.as_deref());
                println!(
                    "  -> {} {} | счёт {} | дата {}",
                    p.kind.tx_kind(),
                    p.amount,
                    account.as_deref().unwrap_or("НЕ ОПРЕДЕЛЁН (задайте default_account или accounts)"),
                    operation_date(m, msg)
                );
            }
        }
        if let Some(e) = journal.get(msg.rowid)? {
            println!("  журнал: {} {}", e.status.label(), detail(&e, &cfg.api_url));
        }
        println!();
    }
    Ok(())
}

pub fn cmd_log(cfg: &Config, n: usize, status: Option<Status>) -> Result<()> {
    let entries = Journal::open(&app_dir())?.last(n, status)?;
    if entries.is_empty() {
        match status {
            Some(st) => println!("В журнале нет записей «{}».", st.label()),
            None => println!("Журнал пуст: сообщения от банка ещё не обрабатывались."),
        }
    }
    for e in &entries {
        println!("{}", entry_line(e, e.status.label(), &cfg.api_url));
    }
    Ok(())
}

/// Подпись счётчика в `status`.
fn count_label(st: Status) -> &'static str {
    match st {
        Status::Created => "создано",
        Status::Matched => "найдено ручных",
        Status::AlreadySent => "уже было",
        Status::Deleted => "удалено в бухгалтерии",
        Status::Rejected => "отклонено",
        Status::Unrecognized => "не распознано",
        Status::Ignored => "кодов",
        Status::Old => "старых",
    }
}

fn status_text(state: &State, counts: &[(Status, i64)]) -> String {
    let t = |t: &DateTime<Local>| t.format("%d.%m %H:%M").to_string();
    // последний запуск остановился, если его ошибка ещё не исправлена
    let stopped = state.last_error.as_ref().is_some_and(|e| !e.resolved);
    let run = match (&state.last_run_at, &state.last_ok_at) {
        (None, _) => "ещё не было".to_string(),
        (Some(at), _) if !stopped => format!("{} — ок", t(at)),
        (Some(at), Some(ok)) => format!("{} — остановка (последний успешный {})", t(at), t(ok)),
        (Some(at), None) => format!("{} — остановка", t(at)),
    };
    let error = match &state.last_error {
        None => "нет".to_string(),
        Some(e) => format!("{} — {}{}", t(&e.at), short(&e.text, 120), if e.resolved { " (исправлено)" } else { "" }),
    };
    let sms = match (state.last_rowid, &state.last_received_at) {
        (None, _) => "—".to_string(),
        (Some(r), None) => format!("#{r}"),
        (Some(r), Some(at)) => format!("#{r}, получено {}", t(at)),
    };
    let queue = match state.failed.as_slice() {
        [] => "0".to_string(),
        q => format!("{} ({})", q.len(), q.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(", ")),
    };
    // основные счётчики — всегда, остальные — если были
    let always = [Status::Created, Status::Matched, Status::Unrecognized, Status::Old];
    let order = [
        Status::Created,
        Status::Matched,
        Status::AlreadySent,
        Status::Deleted,
        Status::Rejected,
        Status::Unrecognized,
        Status::Ignored,
        Status::Old,
    ];
    let week = order
        .into_iter()
        .filter_map(|st| {
            let n = counts.iter().find(|(s, _)| *s == st).map_or(0, |(_, n)| *n);
            (n > 0 || always.contains(&st)).then(|| format!("{} {n}", count_label(st)))
        })
        .collect::<Vec<_>>()
        .join(" · ");
    [
        ("Последний запуск:", run),
        ("Последняя ошибка:", error),
        ("Последнее SMS:", sms),
        ("Очередь повторов:", queue),
        ("За 7 дней:", week),
    ]
    .iter()
    .map(|(k, v)| format!("{k:<19}{v}\n"))
    .collect()
}

/// Только state.json и app.db: без бухгалтерии и chat.db, работает и без Full Disk Access.
pub fn cmd_status() -> Result<()> {
    let dir = app_dir();
    fs::create_dir_all(&dir)?;
    let counts = Journal::open(&dir)?.counts_since(now() - Duration::days(7))?;
    print!("{}", status_text(&load_state(&dir), &counts));
    Ok(())
}

/// Что делать дальше после сообщения.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// не от банка: в журнал не пишется
    Skip,
    /// сообщение слишком свежее: остановить проход и вернуться к нему позже
    Wait,
    /// обработано
    Done(Box<Entry>),
}

/// Строка журнала для сообщения; коды из ignore_patterns не храним.
fn entry(msg: &Msg, status: Status, now: DateTime<Local>) -> Entry {
    Entry {
        rowid: msg.rowid,
        guid: msg.guid.clone(),
        sender: msg.handle.clone(),
        received_at: msg.date,
        text: if status == Status::Ignored { String::new() } else { msg.text.clone() },
        status,
        reason: None,
        kind: None,
        amount: None,
        account_code: None,
        occurred_on: None,
        transaction_id: None,
        processed_at: now,
    }
}

/// Итог отправки: статус, операция, текст отказа.
fn outcome(s: Reply) -> (Status, Option<String>, Option<String>) {
    match s {
        Reply::Created(id) => (Status::Created, Some(id), None),
        Reply::Matched(id) => (Status::Matched, Some(id), None),
        Reply::AlreadySent(id) => (Status::AlreadySent, Some(id), None),
        Reply::Deleted(id) => {
            (Status::Deleted, id, Some("операцию по этому SMS удалили в бухгалтерии: не заводится".into()))
        }
        Reply::Rejected(why) => (Status::Rejected, None, Some(why)),
        // перебор кандидатов не возвращает Gone; если всё же — повторим в следующий раз
        Reply::Gone => (Status::Rejected, None, Some("операцию удалили во время обработки".into())),
    }
}

/// Один проход `run`.
struct Run<'a> {
    cfg: &'a Config,
    m: &'a Matcher,
    ledger: &'a dyn Ledger,
    journal: &'a Journal,
    notifier: &'a dyn Notifier,
    /// каталог state.json и логов
    dir: &'a Path,
    dry: bool,
    now: DateTime<Local>,
    /// отсекать старые сообщения; выключено при `--from-rowid`
    cutoff: bool,
    /// сухой прогон: SMS, «прикреплённые» в этом проходе — (операция, guid, направление)
    dry_links: RefCell<Vec<(String, String, Kind)>>,
}

impl Run<'_> {
    /// Старое сообщение: получено намного раньше последнего обработанного — подкачка истории iCloud, а не новое SMS.
    fn is_old(&self, msg: &Msg, state: &State, retry: bool) -> bool {
        let days = self.cfg.old_message_days;
        self.cutoff
            && !retry
            && days > 0
            && state.last_received_at.is_some_and(|last| msg.date < last - Duration::days(days))
    }

    /// Одно сообщение. `retry` — оно из очереди повторов. `Err` — остановка (сеть, сервер, токен): повторим в следующий запуск.
    fn process(&self, msg: &Msg, state: &State, retry: bool) -> Result<Outcome> {
        if !msg.bank {
            return Ok(Outcome::Skip);
        }
        if self.m.ignored(&msg.text) {
            return Ok(Outcome::Done(Box::new(entry(msg, Status::Ignored, self.now))));
        }
        let Some(p) = self.m.parse(&msg.text) else {
            return Ok(Outcome::Done(Box::new(entry(msg, Status::Unrecognized, self.now))));
        };
        // Свежее сообщение подождёт: вдруг вы сейчас вносите его руками
        if self.cfg.delay_minutes > 0
            && self.now.signed_duration_since(msg.date) < Duration::minutes(self.cfg.delay_minutes)
        {
            println!(
                "Сообщение #{} свежее (меньше {} мин), вернёмся к нему в следующий раз.",
                msg.rowid, self.cfg.delay_minutes
            );
            return Ok(Outcome::Wait);
        }
        let date = operation_date(self.m, msg);
        let account = self.cfg.resolve_account(p.account_hint.as_deref());
        let mut e = Entry {
            kind: Some(p.kind),
            amount: Some(p.amount.to_string()),
            account_code: account.clone(),
            occurred_on: Some(date),
            ..entry(msg, Status::Old, self.now)
        };
        if self.is_old(msg, state, retry) {
            e.reason = Some("старое сообщение (подкачка истории?)".into());
            return Ok(Outcome::Done(Box::new(e)));
        }
        let Some(account) = account else {
            e.status = Status::Rejected;
            e.reason = Some(format!(
                "не определён счёт (цифры в SMS: {:?}); задайте default_account или accounts",
                p.account_hint
            ));
            return Ok(Outcome::Done(Box::new(e)));
        };
        (e.status, e.transaction_id, e.reason) = self.send(msg, &p, &account, date)?;
        Ok(Outcome::Done(Box::new(e)))
    }

    /// Найти SMS в бухгалтерии или ручную операцию для него, иначе создать.
    fn send(
        &self,
        msg: &Msg,
        p: &Parsed,
        account: &str,
        date: NaiveDate,
    ) -> Result<(Status, Option<String>, Option<String>)> {
        let tol = self.cfg.date_tolerance_days.max(0);
        let mut existing =
            self.ledger.transactions_around(account, date - Duration::days(tol), date + Duration::days(tol))?;
        let probe =
            Probe { guid: msg.guid.clone(), kind: p.kind, amount: p.amount, account: account.to_string(), date };
        let verdict = {
            let links = self.dry_links.borrow();
            for (id, guid, _) in links.iter() {
                if let Some(t) = existing.iter_mut().find(|t| t.id == *id) {
                    t.sms_guids.push(guid.clone());
                }
            }
            let side = |g: &str| {
                links.iter().find(|l| l.1 == g).map(|l| l.2).or_else(|| self.journal.kind_of(g).ok().flatten())
            };
            judge(&existing, &probe, tol, self.cfg.skip_if_already_entered, side)
        };
        match verdict {
            Verdict::AlreadySent(id) => Ok((Status::AlreadySent, Some(id), None)),
            Verdict::Entered(ids) if self.dry => {
                self.dry_links.borrow_mut().push((ids[0].clone(), msg.guid.clone(), p.kind));
                Ok((Status::Matched, Some(ids[0].clone()), None))
            }
            Verdict::New if self.dry => Ok((Status::Created, None, None)),
            Verdict::Entered(ids) => {
                let r = external_ref(self.cfg, msg);
                for id in &ids {
                    match self.ledger.attach(id, &r)? {
                        // кандидата удалили, пока шла обработка: следующий
                        Reply::Gone => continue,
                        s => return Ok(outcome(s)),
                    }
                }
                Ok(outcome(self.ledger.create(&build_payload(self.cfg, p, msg, account, date))?))
            }
            Verdict::New => Ok(outcome(self.ledger.create(&build_payload(self.cfg, p, msg, account, date))?)),
        }
    }

    /// Подпись в выводе: сухой прогон говорит, что было бы сделано.
    fn label(&self, e: &Entry) -> &'static str {
        match (self.dry, e.status) {
            (true, Status::Created) => "будет создано",
            (true, Status::Matched) => "будет прикреплено",
            (true, Status::AlreadySent) => "уже отправлено",
            (_, st) => st.label(),
        }
    }

    /// Итог сообщения: вывод, логи, очередь повторов, журнал, уведомление.
    fn record(&self, msg: &Msg, e: &Entry, state: &mut State) -> Result<()> {
        println!("{}", entry_line(e, self.label(e), &self.cfg.api_url));
        if e.status == Status::Unrecognized && !self.dry {
            log_line(
                self.dir,
                "unparsed.log",
                &format!("{}\t#{}\t{:?}", msg.date.format("%F %T"), msg.rowid, msg.text),
            )?;
        }
        if e.status == Status::Rejected {
            let why = e.reason.as_deref().unwrap_or_default();
            eprintln!("ОТКЛОНЕНО #{}: {why}", msg.rowid);
            if !self.dry {
                if self.now.signed_duration_since(msg.date) > Duration::days(RETRY_DAYS) {
                    state.failed.retain(|r| *r != msg.rowid);
                    eprintln!("  сообщению больше {RETRY_DAYS} дней: больше не повторяю, внесите вручную");
                } else if !state.failed.contains(&msg.rowid) {
                    state.failed.push(msg.rowid);
                    log_line(
                        self.dir,
                        "failed.log",
                        &format!("{}\t#{}\t{why}\t{:?}", msg.date.format("%F %T"), msg.rowid, msg.text),
                    )?;
                    eprintln!("  оставлено в очереди: повторю при следующем запуске, когда поправите настройки");
                    self.notifier.notify(TITLE, &format!("SMS не заведено: {}. См. failed.log", short(why, 80)));
                }
            }
        } else {
            state.failed.retain(|r| *r != msg.rowid);
        }
        state.last_received_at = state.last_received_at.max(Some(msg.date));
        if self.dry {
            return Ok(());
        }
        self.journal.write(e)?;
        if let (Status::Created, true, Some(k), Some(amount)) = (e.status, self.cfg.notify_created, e.kind, &e.amount) {
            let account = e.account_code.as_deref().unwrap_or_default();
            let text = format!("Из SMS: {} {amount} {}, {account} — проверьте категорию", k.word(), self.cfg.currency);
            self.notifier.notify(TITLE, &text);
        }
        Ok(())
    }

    /// Сообщения по порядку ROWID; после каждого — журнал, затем state.json.
    fn process_all(&self, msgs: &[Msg], state: &mut State) -> Result<()> {
        let retry = state.failed.clone();
        let mut last = state.last_rowid.unwrap_or_default();
        let mut old = 0;
        let mut failure = None;
        for msg in msgs {
            match self.process(msg, state, retry.contains(&msg.rowid)) {
                Err(e) => {
                    eprintln!("ОШИБКА, остановка: {e:#}");
                    failure = Some(e);
                    break;
                }
                Ok(Outcome::Wait) => break,
                Ok(Outcome::Skip) => state.failed.retain(|r| *r != msg.rowid),
                Ok(Outcome::Done(e)) => {
                    old += usize::from(e.status == Status::Old);
                    self.record(msg, &e, state)?;
                }
            }
            last = last.max(msg.rowid);
            if !self.dry {
                state.last_rowid = Some(last);
                save_state(self.dir, state)?;
            }
        }
        if old > OLD_ALERT && !self.dry {
            let text =
                format!("Пропущено {old} старых SMS — похоже на подкачку истории. Список: {NAME} log --status old");
            self.notifier.notify(TITLE, &text);
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Итог запуска в state.json (для `status`) и уведомление об остановке или восстановлении.
fn finish(state: &mut State, failure: Option<&anyhow::Error>, n: &dyn Notifier) {
    let at = now();
    state.last_run_at = Some(at);
    match failure {
        None => {
            state.last_ok_at = Some(at);
            if let Some(e) = &mut state.last_error {
                e.resolved = true;
            }
            notify::recovered(state, n);
        }
        Some(e) => {
            let text = format!("{e:#}");
            notify::stopped(state, &text, at, n);
            state.last_error = Some(LastError { text, at, resolved: false });
        }
    }
}

/// Ошибка до `cmd_run` (конфиг не читается): уведомление и запись в state.json, как при остановке.
pub fn report_failure(e: &anyhow::Error) {
    let dir = app_dir();
    let mut state = load_state(&dir);
    finish(&mut state, Some(e), &MacNotifier);
    if let Err(e) = save_state(&dir, &state) {
        eprintln!("не записать state.json: {e:#}");
    }
}

pub fn cmd_run(cfg: &Config, dry: bool, from_rowid: Option<i64>) -> Result<()> {
    let dir = app_dir();
    let mut state = load_state(&dir);
    let result = run(cfg, &dir, &mut state, dry, from_rowid);
    if !dry {
        finish(&mut state, result.as_ref().err(), &MacNotifier);
        save_state(&dir, &state)?;
    }
    result
}

fn run(cfg: &Config, dir: &Path, state: &mut State, dry: bool, from_rowid: Option<i64>) -> Result<()> {
    let m = Matcher::new(cfg)?;
    let db = open_db()?;
    if from_rowid.is_some() {
        state.last_rowid = from_rowid;
    }
    let Some(last) = state.last_rowid else {
        let max = max_rowid(&db)?;
        state.last_rowid = Some(max);
        println!("Первый запуск: стартуем с ROWID {max} (историю не импортируем).");
        return Ok(());
    };
    let api = Api::new(cfg)?;
    let journal = Journal::open(dir)?;
    let now = now();
    if dry {
        println!("Сухой прогон: бухгалтерия только читается, журнал и состояние не меняются.");
    } else {
        journal.purge(now - Duration::days(JOURNAL_DAYS))?;
    }
    let msgs = fetch_messages(&db, last, &state.failed, &cfg.senders)?;
    let pass = Run {
        cfg,
        m: &m,
        ledger: &api,
        journal: &journal,
        notifier: &MacNotifier,
        dir,
        dry,
        now,
        cutoff: from_rowid.is_none(),
        dry_links: RefCell::default(),
    };
    pass.process_all(&msgs, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dedupe::Existing, notify::Recorded};
    use anyhow::bail;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use std::{collections::HashMap, path::PathBuf, str::FromStr};

    const WITHDRAWAL: &str = "21/09/26 10:50 A/C X-5019 Withdrawal 60.00 Outstanding Balance 8,195.25 Baht.";

    fn at(day: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, day, h, min, 0).unwrap()
    }

    fn msg(text: &str) -> Msg {
        Msg {
            rowid: 1,
            guid: "G-1".into(),
            text: text.into(),
            date: at(21, 10, 51),
            handle: "KBank".into(),
            bank: true,
        }
    }

    fn msg_n(rowid: i64, text: &str, date: DateTime<Local>) -> Msg {
        Msg { rowid, guid: format!("G-{rowid}"), date, ..msg(text) }
    }

    fn parsed() -> Parsed {
        Parsed {
            kind: Kind::Expense,
            amount: Decimal::from_str("60.00").unwrap(),
            merchant: String::new(),
            account_hint: Some("5019".into()),
        }
    }

    fn cfg() -> Config {
        let mut cfg =
            Config { default_account: "CARD".into(), api_url: "https://budget.example".into(), ..Config::default() };
        cfg.accounts.insert("5019".into(), "CARD_KBANK".into());
        cfg
    }

    fn manual(id: &str, account: &str, date: &str) -> Existing {
        Existing::from_json(
            &json!({"id": id, "tx_kind": "EXPENSE", "account_code": account, "total": "60.00", "occurred_on": date, "items": [], "external_refs": []}),
            "imessage",
        )
        .unwrap()
    }

    /// Бухгалтерия в памяти: заданные операции и ответы, список вызовов.
    #[derive(Default)]
    struct Fake {
        existing: Vec<Existing>,
        /// ответ на прикрепление к операции (нет — прикреплено)
        attach: HashMap<String, Reply>,
        /// ответ на создание (нет — создана "new")
        create: RefCell<Option<Reply>>,
        down: bool,
        calls: RefCell<Vec<String>>,
    }

    impl Ledger for Fake {
        fn transactions_around(&self, account: &str, _: NaiveDate, _: NaiveDate) -> Result<Vec<Existing>> {
            self.calls.borrow_mut().push(format!("A1 {account}"));
            if self.down {
                bail!("запрос /api/transactions: сеть недоступна");
            }
            Ok(self.existing.clone())
        }
        fn create(&self, payload: &Value) -> Result<Reply> {
            assert_eq!(payload["external_ref"]["provider"], "imessage");
            self.calls.borrow_mut().push("A2".into());
            Ok(self.create.borrow().clone().unwrap_or(Reply::Created("new".into())))
        }
        fn attach(&self, id: &str, r: &Value) -> Result<Reply> {
            assert!(r["external_id"].as_str().is_some_and(|g| g.starts_with("G-")));
            self.calls.borrow_mut().push(format!("A3 {id}"));
            Ok(self.attach.get(id).cloned().unwrap_or(Reply::Matched(id.into())))
        }
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    /// Всё для прохода, кроме бухгалтерии; файлы — во временном каталоге.
    struct Env {
        cfg: Config,
        m: Matcher,
        journal: Journal,
        notes: Recorded,
        dir: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{NAME}-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let cfg = cfg();
            Self { m: Matcher::new(&cfg).unwrap(), cfg, journal: Journal::in_memory(), notes: Recorded::default(), dir }
        }

        fn run<'a>(&'a self, ledger: &'a dyn Ledger, dry: bool) -> Run<'a> {
            Run {
                cfg: &self.cfg,
                m: &self.m,
                ledger,
                journal: &self.journal,
                notifier: &self.notes,
                dir: &self.dir,
                dry,
                now: at(21, 12, 0),
                cutoff: true,
                dry_links: RefCell::default(),
            }
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn done(o: Outcome) -> Entry {
        match o {
            Outcome::Done(e) => *e,
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn description_is_clean() {
        let cfg = Config::default();
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        assert_eq!(describe(&cfg, &parsed(), "CARD", d), "[АВТО: проверить] расход 60.00 THB");
        let mut p = parsed();
        p.merchant = "7-ELEVEN".into();
        assert_eq!(describe(&cfg, &p, "CARD", d), "[АВТО: проверить] расход 60.00 THB 7-ELEVEN");
    }

    #[test]
    fn payload_has_external_ref_and_no_marker() {
        let cfg = Config { place_code: Some("seven_eleven".into()), ..Config::default() };
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let mut m = msg(WITHDRAWAL);
        m.guid = "p:0/4F1C".into();
        let v = build_payload(&cfg, &parsed(), &m, "CARD", d);
        assert_eq!(v["tx_kind"], "EXPENSE");
        assert_eq!(v["account_code"], "CARD");
        assert_eq!(v["occurred_on"], "2026-09-21");
        assert_eq!(v["description_lang"], "RU");
        assert_eq!(v["place_code"], "SEVEN_ELEVEN");
        assert!(v.get("for_person_code").is_none());
        assert_eq!(v["items"][0]["category_code"], "AUTO_EXPENSE");
        assert_eq!(v["items"][0]["amount"], "60.00");
        let note = v["items"][0]["note"].as_str().unwrap();
        assert!(note.starts_with("[АВТО: проверить] SMS: 21/09/26 10:50"), "{note}");
        assert!(!note.contains("[sms:"));
        assert_eq!(v["external_ref"]["provider"], "imessage");
        assert_eq!(v["external_ref"]["external_id"], "p:0/4F1C");
        let received = v["external_ref"]["received_at"].as_str().unwrap();
        assert_eq!(DateTime::parse_from_rfc3339(received).unwrap(), m.date);
    }

    #[test]
    fn income_goes_to_income_category() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let mut p = parsed();
        p.kind = Kind::Income;
        let v = build_payload(&Config::default(), &p, &msg("x"), "CARD", d);
        assert_eq!(
            (v["tx_kind"].as_str(), v["items"][0]["category_code"].as_str()),
            (Some("INCOME"), Some("AUTO_INCOME"))
        );
    }

    #[test]
    fn operation_date_prefers_text_but_not_future() {
        let m = Matcher::new(&Config::default()).unwrap();
        let d = |y, mo, da| NaiveDate::from_ymd_opt(y, mo, da).unwrap();
        assert_eq!(operation_date(&m, &msg("20/09/26 23:50 Withdrawal 1.00")), d(2026, 9, 20));
        assert_eq!(operation_date(&m, &msg("25/12/26 10:00 Withdrawal 1.00")), d(2026, 9, 21));
        assert_eq!(operation_date(&m, &msg("Withdrawal 1.00")), d(2026, 9, 21));
    }

    #[test]
    fn old_messages_are_cut_off() {
        let env = Env::new("old");
        let fake = Fake::default();
        let run = env.run(&fake, false);
        let state = State { last_received_at: Some(at(21, 11, 0)), ..State::default() };
        let old = msg_n(5, WITHDRAWAL, at(18, 10, 0));
        let e = done(run.process(&old, &state, false).unwrap());
        assert_eq!((e.status, e.reason.as_deref()), (Status::Old, Some("старое сообщение (подкачка истории?)")));
        assert_eq!((e.amount.as_deref(), e.account_code.as_deref()), (Some("60.00"), Some("CARD_KBANK")));
        assert!(fake.calls().is_empty(), "старое не уходит в бухгалтерию");
        // в пределах old_message_days — не старое
        assert_eq!(
            done(run.process(&msg_n(6, WITHDRAWAL, at(19, 12, 0)), &state, false).unwrap()).status,
            Status::Created
        );
        // из очереди повторов — не отсекается
        assert_eq!(done(run.process(&old, &state, true).unwrap()).status, Status::Created);
        // последнего времени ещё нет
        assert_eq!(done(run.process(&old, &State::default(), false).unwrap()).status, Status::Created);
        // --from-rowid
        let from = Run { cutoff: false, ..env.run(&fake, false) };
        assert_eq!(done(from.process(&old, &state, false).unwrap()).status, Status::Created);
        // отсечка выключена
        let mut off = Env::new("old-off");
        off.cfg.old_message_days = 0;
        assert_eq!(done(off.run(&fake, false).process(&old, &state, false).unwrap()).status, Status::Created);
    }

    #[test]
    fn candidates_are_tried_in_order_then_created() {
        let env = Env::new("candidates");
        let mut fake = Fake {
            existing: vec![manual("far", "CARD_KBANK", "2026-09-22"), manual("near", "CARD_KBANK", "2026-09-21")],
            ..Fake::default()
        };
        fake.attach.insert("near".into(), Reply::Gone);
        let e = done(env.run(&fake, false).process(&msg(WITHDRAWAL), &State::default(), false).unwrap());
        assert_eq!((e.status, e.transaction_id.as_deref()), (Status::Matched, Some("far")));
        assert_eq!(fake.calls(), ["A1 CARD_KBANK", "A3 near", "A3 far"]);

        fake.attach.insert("far".into(), Reply::Gone);
        fake.calls.borrow_mut().clear();
        let e = done(env.run(&fake, false).process(&msg(WITHDRAWAL), &State::default(), false).unwrap());
        assert_eq!((e.status, e.transaction_id.as_deref()), (Status::Created, Some("new")));
        assert_eq!(fake.calls(), ["A1 CARD_KBANK", "A3 near", "A3 far", "A2"]);

        // SMS уже прикреплено к другой операции
        fake.attach.insert("near".into(), Reply::AlreadySent("auto".into()));
        let e = done(env.run(&fake, false).process(&msg(WITHDRAWAL), &State::default(), false).unwrap());
        assert_eq!((e.status, e.transaction_id.as_deref()), (Status::AlreadySent, Some("auto")));
    }

    #[test]
    fn deleted_and_rejected_have_reasons() {
        let env = Env::new("deleted");
        let fake = Fake { create: RefCell::new(Some(Reply::Deleted(Some("gone".into())))), ..Fake::default() };
        let e = done(env.run(&fake, false).process(&msg(WITHDRAWAL), &State::default(), false).unwrap());
        assert_eq!((e.status, e.transaction_id.as_deref()), (Status::Deleted, Some("gone")));
        // счёт не определён: без default_account и без цифр в accounts
        let mut env = Env::new("no-account");
        env.cfg.default_account.clear();
        let e = done(env.run(&fake, false).process(&msg("Withdrawal 60.00"), &State::default(), false).unwrap());
        assert_eq!(e.status, Status::Rejected);
        assert!(e.reason.unwrap().starts_with("не определён счёт"));
    }

    #[test]
    fn dry_run_does_not_send_and_remembers_its_matches() {
        let env = Env::new("dry");
        let fake = Fake { existing: vec![manual("a", "CARD_KBANK", "2026-09-21")], ..Fake::default() };
        let run = env.run(&fake, true);
        let mut state = State { last_rowid: Some(0), ..State::default() };
        run.process_all(&[msg_n(1, WITHDRAWAL, at(21, 10, 51)), msg_n(2, WITHDRAWAL, at(21, 10, 55))], &mut state)
            .unwrap();
        // одна ручная операция — одно SMS: второе «будет создано»
        assert_eq!(fake.calls(), ["A1 CARD_KBANK", "A1 CARD_KBANK"]);
        assert!(env.journal.last(10, None).unwrap().is_empty());
        assert!(env.notes.texts().is_empty());
        assert!(!env.dir.join("state.json").exists());
        let first = done(run.process(&msg_n(3, WITHDRAWAL, at(21, 10, 51)), &state, false).unwrap());
        assert_eq!(first.status, Status::Created);
    }

    #[test]
    fn run_writes_journal_then_state_and_notifies() {
        let env = Env::new("run");
        let fake = Fake::default();
        let mut other = msg_n(4, "привет", at(21, 10, 53));
        other.bank = false;
        let msgs = [
            msg_n(1, "OTP 123456 for K PLUS", at(21, 10, 50)),
            msg_n(2, "Your statement is ready for download in K PLUS", at(21, 10, 51)),
            msg_n(3, WITHDRAWAL, at(21, 10, 52)),
            other,
            // свежее: проход останавливается на нём
            msg_n(5, WITHDRAWAL, at(21, 11, 55)),
        ];
        let mut state = State { last_rowid: Some(0), ..State::default() };
        env.run(&fake, false).process_all(&msgs, &mut state).unwrap();

        let rows = env.journal.last(10, None).unwrap();
        let got: Vec<_> = rows.iter().map(|e| (e.rowid, e.status)).collect();
        assert_eq!(got, [(1, Status::Ignored), (2, Status::Unrecognized), (3, Status::Created)]);
        assert_eq!(rows[0].text, "", "коды не храним");
        assert_eq!(rows[2].transaction_id.as_deref(), Some("new"));
        assert_eq!(env.notes.texts(), ["Из SMS: расход 60.00 THB, CARD_KBANK — проверьте категорию"]);
        assert_eq!((state.last_rowid, state.last_received_at), (Some(4), Some(at(21, 10, 52))));
        let saved = load_state(&env.dir);
        assert_eq!((saved.last_rowid, saved.last_received_at), (Some(4), Some(at(21, 10, 52))));
        assert!(fs::read_to_string(env.dir.join("unparsed.log")).unwrap().contains("#2"));

        let mut quiet = Env::new("run-quiet");
        quiet.cfg.notify_created = false;
        quiet.run(&fake, false).process_all(&msgs[2..3], &mut State::default()).unwrap();
        assert!(quiet.notes.texts().is_empty());
    }

    #[test]
    fn rejected_goes_to_queue_and_notifies_once() {
        let env = Env::new("rejected");
        let fake = Fake {
            create: RefCell::new(Some(Reply::Rejected("422 категория AUTO_EXPENSE не найдена".into()))),
            ..Fake::default()
        };
        let m = msg_n(7, WITHDRAWAL, at(21, 10, 51));
        let mut state = State { last_rowid: Some(6), ..State::default() };
        env.run(&fake, false).process_all(std::slice::from_ref(&m), &mut state).unwrap();
        env.run(&fake, false).process_all(std::slice::from_ref(&m), &mut state).unwrap();
        assert_eq!(state.failed, [7]);
        assert_eq!(env.notes.texts(), ["SMS не заведено: 422 категория AUTO_EXPENSE не найдена. См. failed.log"]);
        assert_eq!(env.journal.get(7).unwrap().unwrap().status, Status::Rejected);
        assert_eq!(fs::read_to_string(env.dir.join("failed.log")).unwrap().lines().count(), 1);
        // настройки поправили: повтор проходит, строка журнала перезаписана
        *fake.create.borrow_mut() = None;
        env.run(&fake, false).process_all(std::slice::from_ref(&m), &mut state).unwrap();
        assert!(state.failed.is_empty());
        assert_eq!(env.journal.get(7).unwrap().unwrap().status, Status::Created);
    }

    #[test]
    fn stop_keeps_the_message_for_next_run() {
        let env = Env::new("stop");
        let fake = Fake { down: true, ..Fake::default() };
        let mut state = State { last_rowid: Some(2), ..State::default() };
        let e = env.run(&fake, false).process_all(&[msg_n(3, WITHDRAWAL, at(21, 10, 51))], &mut state).unwrap_err();
        assert!(e.to_string().contains("сеть недоступна"));
        assert_eq!((state.last_rowid, state.last_received_at), (Some(2), None));
        assert!(env.journal.get(3).unwrap().is_none());
    }

    #[test]
    fn many_old_messages_notify_once() {
        let env = Env::new("history");
        let fake = Fake::default();
        let msgs: Vec<Msg> = (1..=21).map(|i| msg_n(i, WITHDRAWAL, at(1, 10, 0))).collect();
        let mut state = State { last_rowid: Some(0), last_received_at: Some(at(21, 10, 0)), ..State::default() };
        env.run(&fake, false).process_all(&msgs, &mut state).unwrap();
        assert_eq!(env.journal.last(100, Some(Status::Old)).unwrap().len(), 21);
        assert_eq!(
            env.notes.texts(),
            [format!("Пропущено 21 старых SMS — похоже на подкачку истории. Список: {NAME} log --status old")]
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn finish_records_stop_and_recovery() {
        let n = Recorded::default();
        let mut state = State::default();
        finish(&mut state, Some(&anyhow::anyhow!("/api/transactions: 401 токен недействителен")), &n);
        let err = state.last_error.clone().unwrap();
        assert_eq!((err.text.as_str(), err.resolved), ("/api/transactions: 401 токен недействителен", false));
        assert!(state.last_run_at.is_some() && state.last_ok_at.is_none());
        finish(&mut state, None, &n);
        assert!(state.last_error.unwrap().resolved);
        assert_eq!(state.last_ok_at, state.last_run_at);
        assert_eq!(n.texts().len(), 2);
        assert_eq!(n.texts()[1], "Импорт SMS снова работает");
    }

    #[test]
    fn log_lines_match_the_spec() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 21);
        let base = Entry {
            kind: Some(Kind::Expense),
            amount: Some("60.00".into()),
            account_code: Some("CARD_KBANK".into()),
            occurred_on: d,
            transaction_id: Some("7b1e".into()),
            ..entry(&msg_n(48211, WITHDRAWAL, at(21, 10, 51)), Status::Created, at(21, 11, 2))
        };
        let url = "https://budget.example/";
        assert_eq!(
            entry_line(&base, base.status.label(), url),
            "#48211  21.09 10:51  расход   60.00 CARD_KBANK 21.09  создано          https://budget.example/transactions/7b1e/edit"
        );
        let rejected = Entry {
            status: Status::Rejected,
            kind: Some(Kind::Income),
            amount: Some("500.00".into()),
            transaction_id: None,
            reason: Some("422 категория AUTO_INCOME не найдена".into()),
            ..base.clone()
        };
        assert_eq!(
            entry_line(&rejected, rejected.status.label(), url),
            "#48211  21.09 10:51  доход   500.00 CARD_KBANK 21.09  отклонено        422 категория AUTO_INCOME не найдена"
        );
        let unknown = entry(
            &msg_n(48220, "Your statement is ready for download in K PLUS", at(21, 13, 2)),
            Status::Unrecognized,
            at(21, 13, 15),
        );
        assert_eq!(
            entry_line(&unknown, unknown.status.label(), url),
            "#48220  21.09 13:02  —                                не распознано    «Your statement is ready for download in…»"
        );
    }

    #[test]
    fn status_lines() {
        let t = |d, h, m| Some(at(d, h, m));
        let state = State {
            last_rowid: Some(48220),
            failed: vec![48219],
            last_received_at: t(21, 13, 2),
            last_run_at: t(24, 14, 5),
            last_ok_at: t(24, 14, 5),
            last_error: Some(LastError {
                text: "401 токен недействителен".into(), at: at(23, 9, 10), resolved: true
            }),
            alert: None,
        };
        let counts = [(Status::Created, 12), (Status::Matched, 30), (Status::Unrecognized, 1), (Status::Rejected, 1)];
        assert_eq!(
            status_text(&state, &counts),
            "Последний запуск:  24.09 14:05 — ок\n\
             Последняя ошибка:  23.09 09:10 — 401 токен недействителен (исправлено)\n\
             Последнее SMS:     #48220, получено 21.09 13:02\n\
             Очередь повторов:  1 (#48219)\n\
             За 7 дней:         создано 12 · найдено ручных 30 · отклонено 1 · не распознано 1 · старых 0\n"
        );
        let empty = status_text(&State::default(), &[]);
        assert!(empty.starts_with("Последний запуск:  ещё не было\n"), "{empty}");
        // остановка в ту же секунду, что и прошлый успешный запуск
        let failed = State {
            last_error: Some(LastError { text: "разбор config.json".into(), at: at(24, 14, 5), resolved: false }),
            last_ok_at: t(23, 9, 0),
            ..state
        };
        assert!(
            status_text(&failed, &[])
                .starts_with("Последний запуск:  24.09 14:05 — остановка (последний успешный 23.09 09:00)\n")
        );
    }
}
