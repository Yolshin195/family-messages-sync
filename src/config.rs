//! Конфиг, состояние и пути.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
};

pub const NAME: &str = env!("CARGO_PKG_NAME");
/// Имя записи в Keychain (сервис — `keychain_service` из конфига).
pub const KEYCHAIN_ACCOUNT: &str = "api-token";

#[derive(Serialize, Deserialize, Clone)]
pub struct Category {
    /// Код: латиница, цифры, `_`, до 40 знаков (в бухгалтерии приводится к верхнему регистру)
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Rule {
    /// "expense" | "income"
    #[serde(rename = "type")]
    pub kind: String,
    /// regex; обязательная группа `amount`, необязательные `merchant` и `account` (последние цифры счёта)
    pub pattern: String,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Адрес Семейной бухгалтерии, например https://budget.example.com
    pub api_url: String,
    /// Сервис Keychain, где лежит API-токен `fbt_…`
    pub keychain_service: String,
    /// Подстроки в handle.id отправителя, без учёта регистра
    pub senders: Vec<String>,
    /// Сообщение, совпавшее с любым из regex, пропускается (OTP)
    pub ignore_patterns: Vec<String>,

    /// Счёт по умолчанию (код из справочника счетов)
    pub default_account: String,
    /// Последние цифры счёта из SMS (`A/C X-5019` → `5019`) → код счёта в бухгалтерии
    pub accounts: BTreeMap<String, String>,
    /// Валюта счетов из SMS (только для текста описания и проверки в `init`)
    pub currency: String,

    /// Категория для расходов, внесённых автоматически (позже разнести вручную)
    pub expense_category: Category,
    /// Категория для доходов, внесённых автоматически
    pub income_category: Category,

    /// Язык оригинала описания: RU / EN / TH
    pub description_lang: String,
    /// Пометка «внесено автоматически, проверить». Идёт в начало описания и в note позиции
    pub review_marker: String,
    /// Шаблон описания: {marker} {kind} {amount} {currency} {merchant} {account} {date}
    pub description_template: String,
    /// Место (код из справочника мест) для всех автоматических операций, если нужно
    pub place_code: Option<String>,
    /// «Для кого» (код из справочника людей) для всех автоматических операций, если нужно
    pub for_person_code: Option<String>,

    /// Не вносить сообщение, пока ему меньше N минут: за это время операцию можно успеть
    /// внести руками, и приложение увидит её и не создаст дубль
    pub delay_minutes: i64,
    /// Насколько дней от даты SMS искать уже внесённую операцию
    pub date_tolerance_days: i64,
    /// Не создавать операцию, если такая же (счёт, направление, сумма, дата) уже внесена вручную
    pub skip_if_already_entered: bool,

    /// `provider` связей SMS с операциями в бухгалтерии. Не менять без причины
    pub provider: String,
    /// Уведомление macOS на каждую созданную операцию
    pub notify_created: bool,
    /// Не заводить сообщение, полученное больше чем на N дней раньше последнего обработанного:
    /// это подкачка истории iCloud, а не новое SMS. 0 — выключить
    pub old_message_days: i64,

    /// Правила разбора, первое совпавшее побеждает
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: "http://localhost:8080".into(),
            keychain_service: NAME.into(),
            senders: vec!["kbank".into(), "k plus".into(), "kplus".into()],
            ignore_patterns: vec!["(?i)otp".into(), "(?i)one[- ]time password".into()],
            default_account: String::new(),
            accounts: BTreeMap::new(),
            currency: "THB".into(),
            expense_category: Category {
                code: "AUTO_EXPENSE".into(),
                name: "Авто: проверить (расход)".into(),
                name_en: Some("Auto: to review (expense)".into()),
            },
            income_category: Category {
                code: "AUTO_INCOME".into(),
                name: "Авто: проверить (доход)".into(),
                name_en: Some("Auto: to review (income)".into()),
            },
            description_lang: "RU".into(),
            review_marker: "[АВТО: проверить]".into(),
            description_template: "{marker} {kind} {amount} {currency} {merchant}".into(),
            place_code: None,
            for_person_code: None,
            delay_minutes: 10,
            date_tolerance_days: 1,
            skip_if_already_entered: true,
            provider: "imessage".into(),
            notify_created: true,
            old_message_days: 2,
            rules: vec![
                // "21/09/26 10:50 A/C X-5019 Withdrawal 60.00 Outstanding Balance 8,195.25 Baht."
                Rule {
                    kind: "expense".into(),
                    pattern: r"(?i)(?:A/C\s+X-(?P<account>\d+)\s+)?\bwithdrawal\s+(?P<amount>\d[\d,]*\.\d{2})".into(),
                },
                // "20/09/26 21:26 A/C X-5019 transferred 120.00 Baht to A/C X-8842 Outstanding Balance 10,002.25 Baht."
                Rule {
                    kind: "expense".into(),
                    pattern: r"(?i)(?:A/C\s+X-(?P<account>\d+)\s+)?\btransferred\s+(?P<amount>\d[\d,]*\.\d{2})".into(),
                },
                // "20/09/26 11:44 A/C X-5019 Deposit 11,088.02 Outstanding Balance 12,282.25 Baht."
                Rule {
                    kind: "income".into(),
                    pattern: r"(?i)(?:A/C\s+X-(?P<account>\d+)\s+)?\b(?:deposit|received|transfer in|credit)\w*\s+(?P<amount>\d[\d,]*\.\d{2})".into(),
                },
            ],
        }
    }
}

impl Config {
    /// Счёт для сообщения: по цифрам из SMS, иначе счёт по умолчанию.
    pub fn resolve_account(&self, hint: Option<&str>) -> Option<String> {
        let mapped = hint.and_then(|h| self.accounts.get(h));
        let code = mapped.unwrap_or(&self.default_account).trim().to_uppercase();
        (!code.is_empty()).then_some(code)
    }

    /// Все коды счетов, которые приложение может использовать.
    pub fn account_codes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.accounts.values().map(|c| c.trim().to_uppercase()).collect();
        if !self.default_account.trim().is_empty() {
            v.push(self.default_account.trim().to_uppercase());
        }
        v.sort();
        v.dedup();
        v
    }
}

/// Последняя ошибка `run` (для `status`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastError {
    pub text: String,
    pub at: DateTime<Local>,
    /// после неё был запуск без остановки
    pub resolved: bool,
}

/// Последнее уведомление об остановке: чтобы не повторять его и сообщить, когда импорт заработает.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// первые 40 символов причины
    pub reason: String,
    pub at: DateTime<Local>,
    /// импорт стоит: о восстановлении ещё не сообщали
    pub stopped: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub last_rowid: Option<i64>,
    /// Сообщения, которые бухгалтерия отклонила (нет счёта, категории): повторяются при каждом запуске,
    /// пока не уйдут, чтобы после исправления настроек ничего не потерялось
    #[serde(default)]
    pub failed: Vec<i64>,
    /// Время получения последнего обработанного сообщения от банка: старше него — подкачка истории
    #[serde(default)]
    pub last_received_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub last_run_at: Option<DateTime<Local>>,
    /// Последний запуск без остановки
    #[serde(default)]
    pub last_ok_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub last_error: Option<LastError>,
    #[serde(default)]
    pub alert: Option<Alert>,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME не задан"))
}

pub fn app_dir() -> PathBuf {
    home().join(".config").join(NAME)
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub fn read_config() -> Option<Config> {
    serde_json::from_str(&fs::read_to_string(config_path()).ok()?).ok()
}

pub fn write_config(cfg: &Config) -> Result<()> {
    fs::create_dir_all(app_dir())?;
    fs::write(config_path(), serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Загружает конфиг; если его нет — в терминале запускает первичную настройку.
/// Второе значение: true, если настройка была запущена.
pub fn load_config(setup: impl FnOnce() -> Result<()>) -> Result<(Config, bool)> {
    let path = config_path();
    let mut setup_ran = false;
    if !path.exists() {
        if !std::io::stdin().is_terminal() {
            bail!("Конфиг {} не найден. Запустите в терминале: {NAME} setup", path.display());
        }
        println!("Конфиг не найден — первая настройка.");
        setup()?;
        setup_ran = true;
    }
    let cfg = serde_json::from_str(&fs::read_to_string(&path)?).with_context(|| format!("разбор {}", path.display()))?;
    Ok((cfg, setup_ran))
}

/// Состояние из `dir/state.json` (обычно `app_dir()`).
pub fn load_state(dir: &Path) -> State {
    fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(dir: &Path, state: &State) -> Result<()> {
    // через временный файл: оборванная запись не сбросит позицию сообщений
    let tmp = dir.join("state.json.tmp");
    fs::write(&tmp, serde_json::to_string(state)?)?;
    fs::rename(tmp, dir.join("state.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_state_with_claimed_is_read_and_claimed_is_dropped() {
        let old = r#"{"last_rowid":48220,"claimed":[{"id":"a","guid":"G-1","date":"2026-09-21"}],"failed":[48190]}"#;
        let state: State = serde_json::from_str(old).unwrap();
        assert_eq!((state.last_rowid, state.failed.as_slice()), (Some(48220), &[48190][..]));
        assert!(state.last_received_at.is_none() && state.alert.is_none());
        let saved = serde_json::to_string(&state).unwrap();
        assert!(!saved.contains("claimed"), "{saved}");
    }

    #[test]
    fn new_config_fields_have_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"default_account":"CARD"}"#).unwrap();
        assert_eq!((cfg.provider.as_str(), cfg.notify_created, cfg.old_message_days), ("imessage", true, 2));
    }
}
