//! Обработка новых сообщений: разбор, проверка дублей, создание операций.

use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, io::Write};

use crate::{
    api::{Api, CreateError},
    config::{Claim, Config, app_dir, load_state, save_state},
    dedupe::{Probe, Verdict, judge, sms_marker},
    messages::{Msg, fetch_messages, max_rowid, open_db},
    parse::{Matcher, Parsed},
};

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

/// Тело `POST /api/transactions`.
pub fn build_payload(cfg: &Config, p: &Parsed, msg: &Msg, account: &str, date: NaiveDate) -> Value {
    let category = match p.kind {
        crate::parse::Kind::Expense => &cfg.expense_category,
        crate::parse::Kind::Income => &cfg.income_category,
    };
    let sms: String = msg.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
    let mut v = json!({
        "occurred_on": date.to_string(),
        "tx_kind": p.kind.tx_kind(),
        "account_code": account,
        "items": [{
            "category_code": category.code.trim().to_uppercase(),
            "amount": p.amount.to_string(),
            // метка нужна для защиты от дублей; остальное — чтобы при проверке видеть исходный SMS
            "note": format!("{} {} SMS: {}", sms_marker(&msg.guid), cfg.review_marker, sms),
        }],
        "description": describe(cfg, p, account, date),
        "description_lang": cfg.description_lang.to_uppercase(),
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

fn log_line(file: &str, line: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(app_dir().join(file))?;
    writeln!(f, "{line}")?;
    Ok(())
}

pub fn cmd_show(cfg: &Config, m: &Matcher, n: usize) -> Result<()> {
    let db = open_db()?;
    let banks: Vec<Msg> = fetch_messages(&db, 0, &[], &cfg.senders)?.into_iter().filter(|m| m.bank).collect();
    if banks.is_empty() {
        println!("Сообщений от банка не найдено — проверьте `senders` в конфиге.");
    }
    for msg in &banks[banks.len().saturating_sub(n)..] {
        println!("#{} {} [{}]", msg.rowid, msg.date.format("%Y-%m-%d %H:%M"), msg.handle);
        println!("  {}", msg.text);
        match m.parse(&msg.text) {
            None => println!("  -> не распознано\n"),
            Some(p) => {
                let account = cfg.resolve_account(p.account_hint.as_deref());
                println!(
                    "  -> {} {} | счёт {} | дата {}\n",
                    p.kind.tx_kind(),
                    p.amount,
                    account.as_deref().unwrap_or("НЕ ОПРЕДЕЛЁН (задайте default_account или accounts)"),
                    operation_date(m, msg)
                );
            }
        }
    }
    Ok(())
}

/// Чем закончилась обработка одного сообщения.
enum Outcome {
    /// обработано (создано, дубль, не банк, не распознано): двигаемся дальше
    Done,
    /// сообщение слишком свежее: остановиться и вернуться к нему позже
    Wait,
    /// сеть, сервер, токен: остановиться, повторим при следующем запуске
    Stop(anyhow::Error),
    /// бухгалтерия отклонила или не хватает настроек: сообщение остаётся в очереди повторов
    Rejected(String),
}

fn process(
    cfg: &Config,
    m: &Matcher,
    api: &Api,
    msg: &Msg,
    dry: bool,
    now: DateTime<Local>,
    claimed: &mut HashMap<String, String>,
    new_claims: &mut Vec<Claim>,
) -> Result<Outcome> {
    // OTP и прочее из ignore_patterns пропускаем молча (коды не пишем в лог)
    if !msg.bank || m.ignored(&msg.text) {
        return Ok(Outcome::Done);
    }
    let Some(p) = m.parse(&msg.text) else {
        if dry {
            println!("? не распознано: {:?}", msg.text);
        } else {
            log_line("unparsed.log", &format!("{}\t#{}\t{:?}", msg.date.format("%F %T"), msg.rowid, msg.text))?;
        }
        return Ok(Outcome::Done);
    };
    // Свежее сообщение подождёт: вдруг вы сейчас вносите его руками
    if cfg.delay_minutes > 0 && now.signed_duration_since(msg.date) < Duration::minutes(cfg.delay_minutes) {
        println!("Сообщение #{} свежее (меньше {} мин), вернёмся к нему в следующий раз.", msg.rowid, cfg.delay_minutes);
        return Ok(Outcome::Wait);
    }
    let date = operation_date(m, msg);
    let Some(account) = cfg.resolve_account(p.account_hint.as_deref()) else {
        return Ok(Outcome::Rejected(format!("не определён счёт (цифры в SMS: {:?}); задайте default_account или accounts", p.account_hint)));
    };

    let tol = cfg.date_tolerance_days.max(0);
    let existing = match api.transactions_around(&account, date - Duration::days(tol), date + Duration::days(tol)) {
        Ok(v) => v,
        Err(e) => return Ok(Outcome::Stop(e)),
    };
    let probe = Probe { guid: msg.guid.clone(), kind: p.kind, amount: p.amount, account: account.clone(), date };
    match judge(&existing, &probe, claimed, tol, cfg.skip_if_already_entered) {
        Verdict::AlreadySent(id) => println!("ДУБЛЬ #{}: уже отправлен, операция {id}", msg.rowid),
        Verdict::Entered(id) => {
            println!("ДУБЛЬ #{}: {} {} {account} {date} уже внесён вручную, операция {id}", msg.rowid, p.kind.tx_kind(), p.amount);
            if claimed.insert(id.clone(), msg.guid.clone()).is_none() {
                new_claims.push(Claim { id, guid: msg.guid.clone(), date: date.to_string() });
            }
        }
        Verdict::New if dry => println!("DRY {} {} {account} {date} | {}", p.kind.tx_kind(), p.amount, describe(cfg, &p, &account, date)),
        Verdict::New => match api.create_transaction(&build_payload(cfg, &p, msg, &account, date)) {
            Ok(id) => println!("OK {} {} {account} ({date}) операция {id}", p.kind.tx_kind(), p.amount),
            Err(CreateError::Rejected(why)) => return Ok(Outcome::Rejected(why)),
            Err(CreateError::Transient(e)) => return Ok(Outcome::Stop(e)),
        },
    }
    Ok(Outcome::Done)
}

/// Сколько дней очередь повторов держит отклонённое сообщение
const RETRY_DAYS: i64 = 14;

pub fn cmd_run(cfg: &Config, m: &Matcher, dry: bool, from_rowid: Option<i64>) -> Result<()> {
    let db = open_db()?;
    let mut state = load_state();
    if from_rowid.is_some() {
        state.last_rowid = from_rowid;
    }
    let Some(mut last) = state.last_rowid else {
        let max = max_rowid(&db)?;
        state.last_rowid = Some(max);
        if !dry {
            save_state(&state)?;
        }
        println!("Первый запуск: стартуем с ROWID {max} (историю не импортируем).");
        return Ok(());
    };

    // «занятые» ручные операции помним месяц с небольшим
    let cutoff = (Local::now().date_naive() - Duration::days(45)).to_string();
    state.claimed.retain(|c| c.date >= cutoff);
    let mut claimed: HashMap<String, String> = state.claimed.iter().map(|c| (c.id.clone(), c.guid.clone())).collect();

    let api = Api::new(cfg)?;
    let now = Local::now();
    let mut failure: Option<anyhow::Error> = None;

    for msg in fetch_messages(&db, last, &state.failed, &cfg.senders)? {
        let mut new_claims = Vec::new();
        match process(cfg, m, &api, &msg, dry, now, &mut claimed, &mut new_claims)? {
            Outcome::Wait => break,
            Outcome::Stop(e) => {
                eprintln!("ОШИБКА, остановка: {e:#}");
                failure = Some(e);
                break;
            }
            Outcome::Rejected(why) => {
                eprintln!("ОТКЛОНЕНО #{}: {why}", msg.rowid);
                if !dry {
                    if now.signed_duration_since(msg.date) > Duration::days(RETRY_DAYS) {
                        state.failed.retain(|r| *r != msg.rowid);
                        eprintln!("  сообщению больше {RETRY_DAYS} дней: больше не повторяю, внесите вручную");
                    } else if !state.failed.contains(&msg.rowid) {
                        state.failed.push(msg.rowid);
                        log_line("failed.log", &format!("{}\t#{}\t{why}\t{:?}", msg.date.format("%F %T"), msg.rowid, msg.text))?;
                        eprintln!("  оставлено в очереди: повторю при следующем запуске, когда поправите настройки");
                    }
                }
            }
            Outcome::Done => state.failed.retain(|r| *r != msg.rowid),
        }
        state.claimed.extend(new_claims);
        last = last.max(msg.rowid);
        if !dry {
            state.last_rowid = Some(last);
            save_state(&state)?;
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Kind;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn msg(text: &str) -> Msg {
        Msg { rowid: 1, guid: "G-1".into(), text: text.into(), date: Local.with_ymd_and_hms(2026, 9, 21, 10, 51, 0).unwrap(), handle: "KBank".into(), bank: true }
    }

    fn parsed() -> Parsed {
        Parsed { kind: Kind::Expense, amount: Decimal::from_str("60.00").unwrap(), merchant: String::new(), account_hint: Some("5019".into()) }
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
    fn payload_has_marker_category_and_defaults() {
        let mut cfg = Config::default();
        cfg.place_code = Some("seven_eleven".into());
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let m = msg("21/09/26 10:50 A/C X-5019 Withdrawal 60.00 Outstanding Balance 8,195.25 Baht.");
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
        assert!(note.starts_with("[sms:G-1] [АВТО: проверить] SMS: 21/09/26"));
    }

    #[test]
    fn income_goes_to_income_category() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let mut p = parsed();
        p.kind = Kind::Income;
        let v = build_payload(&Config::default(), &p, &msg("x"), "CARD", d);
        assert_eq!((v["tx_kind"].as_str(), v["items"][0]["category_code"].as_str()), (Some("INCOME"), Some("AUTO_INCOME")));
    }

    #[test]
    fn operation_date_prefers_text_but_not_future() {
        let m = Matcher::new(&Config::default()).unwrap();
        let d = |y, mo, da| NaiveDate::from_ymd_opt(y, mo, da).unwrap();
        assert_eq!(operation_date(&m, &msg("20/09/26 23:50 Withdrawal 1.00")), d(2026, 9, 20));
        assert_eq!(operation_date(&m, &msg("25/12/26 10:00 Withdrawal 1.00")), d(2026, 9, 21));
        assert_eq!(operation_date(&m, &msg("Withdrawal 1.00")), d(2026, 9, 21));
    }
}
