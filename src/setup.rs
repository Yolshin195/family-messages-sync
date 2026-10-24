//! Интерактивная настройка, сброс и проверка связи с бухгалтерией.

use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
};

use crate::{
    api::Api,
    config::{Config, NAME, app_dir, config_path, read_config, write_config},
    keychain,
};

fn ask(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        bail!("ввод прерван (нет данных на stdin)");
    }
    let line = line.trim();
    Ok(if line.is_empty() { default.to_string() } else { line.to_string() })
}

fn ask_required(label: &str, default: &str) -> Result<String> {
    loop {
        let v = ask(label, default)?;
        if !v.is_empty() {
            return Ok(v);
        }
        println!("Значение обязательно.");
    }
}

fn ask_yes_no(label: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    let v = ask(&format!("{label} ({hint})"), "")?.to_lowercase();
    Ok(match v.as_str() {
        "" => default_yes,
        "y" | "yes" | "д" | "да" => true,
        _ => false,
    })
}

/// "5019=CARD_KBANK, 1234=CASH" -> {"5019": "CARD_KBANK", "1234": "CASH"}
pub fn parse_accounts(s: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((digits, code)) = part.split_once('=') else {
            bail!("ожидается формат ЦИФРЫ=КОД_СЧЁТА, а не {part:?}");
        };
        let (digits, code) = (digits.trim(), code.trim().to_uppercase());
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) || code.is_empty() {
            bail!("ожидается формат ЦИФРЫ=КОД_СЧЁТА, а не {part:?}");
        }
        map.insert(digits.to_string(), code);
    }
    Ok(map)
}

fn format_accounts(m: &BTreeMap<String, String>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
}

/// Спрашивает основные параметры и записывает конфиг. Если конфиг уже есть, текущие значения — по умолчанию.
pub fn cmd_setup() -> Result<()> {
    let mut cfg = read_config().unwrap_or_default();
    println!("Настройка {NAME} (Enter — оставить значение в скобках)");
    cfg.api_url = ask("Адрес Семейной бухгалтерии", &cfg.api_url)?;
    let senders = ask("Отправители SMS (через запятую)", &cfg.senders.join(", "))?;
    cfg.senders = senders.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();

    println!("Счёт по умолчанию — код из справочника счетов (например CARD_KBANK).");
    cfg.default_account = ask_required("Счёт по умолчанию", &cfg.default_account)?.to_uppercase();
    println!("Если SMS приходят с нескольких счетов: последние цифры из SMS (A/C X-5019) = код счёта.");
    loop {
        match parse_accounts(&ask("Соответствие счетов (через запятую, можно пусто)", &format_accounts(&cfg.accounts))?)
        {
            Ok(m) => {
                cfg.accounts = m;
                break;
            }
            Err(e) => println!("{e}"),
        }
    }
    cfg.expense_category.code =
        ask_required("Категория для автоматических расходов", &cfg.expense_category.code)?.to_uppercase();
    cfg.income_category.code =
        ask_required("Категория для автоматических доходов", &cfg.income_category.code)?.to_uppercase();

    write_config(&cfg)?;
    println!("Конфиг сохранён: {}", config_path().display());
    println!("Описание, пометку, место, «для кого», паузу и правила разбора можно менять в этом файле.");

    let have_token = keychain::exists(&cfg.keychain_service);
    if !have_token || ask_yes_no("API-токен уже есть. Заменить?", false)? {
        println!("API-токен (создайте в бухгалтерии: Профиль → Безопасность; ввод скрыт, сохраняется в Keychain):");
        keychain::set(&cfg.keychain_service)?;
    }

    println!("Проверяю токен, категории и счета...");
    if let Err(e) = cmd_init(&cfg) {
        eprintln!("Не удалось: {e:#}\nКонфиг сохранён; исправьте настройки и повторите: {NAME} setup");
    }
    Ok(())
}

/// Удаляет конфиг, состояние и токен из Keychain, затем настраивает заново.
pub fn cmd_reset(yes: bool) -> Result<()> {
    if !yes
        && !ask_yes_no(
            "Удалить конфиг, состояние и токен из Keychain и настроить заново? Позиция сообщений сбросится: история не импортируется",
            false,
        )?
    {
        println!("Отменено.");
        return Ok(());
    }
    if let Some(cfg) = read_config() {
        keychain::delete(&cfg.keychain_service);
    }
    for f in ["config.json", "state.json", "app.db", "unparsed.log", "failed.log", "unparsed.log.1", "failed.log.1"] {
        let _ = fs::remove_file(app_dir().join(f));
    }
    println!("Конфигурация удалена.");
    cmd_setup()
}

/// Проверка связи: токен, версия бухгалтерии, категории для автоматических операций (создаются, если их нет), счета.
pub fn cmd_init(cfg: &Config) -> Result<()> {
    let api = Api::new(cfg)?;
    api.check_external_refs()?;
    println!("бухгалтерия: связи SMS поддерживаются (ок)");
    api.ensure_category(&cfg.expense_category, "EXPENSE")?;
    api.ensure_category(&cfg.income_category, "INCOME")?;
    let codes = cfg.account_codes();
    if codes.is_empty() {
        bail!("не задан счёт: укажите default_account (и при необходимости accounts) в конфиге");
    }
    let mut problems = Vec::new();
    for code in codes {
        match api.account_info(&code)? {
            None => problems.push(format!("счёт {code} не найден в бухгалтерии")),
            Some(a) if a.archived => problems.push(format!("счёт {code} в архиве")),
            Some(a) if !a.currency.eq_ignore_ascii_case(&cfg.currency) => problems.push(format!(
                "счёт {code} в валюте {}, а в SMS {}: суммы запишутся как есть в валюте счёта",
                a.currency, cfg.currency
            )),
            Some(a) => println!("счёт {code}: {} (ок)", a.currency),
        }
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_parse() {
        let m = parse_accounts("5019=card_kbank, 1234 = CASH").unwrap();
        assert_eq!(format_accounts(&m), "1234=CASH, 5019=CARD_KBANK");
        assert!(parse_accounts("").unwrap().is_empty());
        assert!(parse_accounts("5019").is_err());
        assert!(parse_accounts("abc=CARD").is_err());
    }
}
