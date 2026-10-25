//! Разбор текста банковского SMS.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::config::Config;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Expense,
    Income,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "expense" => Some(Self::Expense),
            "income" => Some(Self::Income),
            _ => None,
        }
    }

    /// Значение `tx_kind` в API бухгалтерии.
    pub fn tx_kind(self) -> &'static str {
        match self {
            Self::Expense => "EXPENSE",
            Self::Income => "INCOME",
        }
    }

    pub fn from_tx_kind(s: &str) -> Option<Self> {
        [Self::Expense, Self::Income].into_iter().find(|k| k.tx_kind() == s)
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Expense => "расход",
            Self::Income => "доход",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub kind: Kind,
    pub amount: Decimal,
    pub merchant: String,
    /// последние цифры счёта из SMS (`A/C X-5019` → `5019`)
    pub account_hint: Option<String>,
    /// остаток счёта после операции (`Outstanding Balance 8,195.25` → `8195.25`)
    pub balance: Option<Decimal>,
}

pub struct Matcher {
    ignore: Vec<Regex>,
    rules: Vec<(Kind, Regex)>,
    /// дата и необязательное время в начале SMS: `21/09/26 10:50`
    date: Regex,
    balance: Option<Regex>,
}

/// Число из SMS: `8,195.25` → `8195.25`.
fn number(s: &str) -> Option<Decimal> {
    Decimal::from_str(&s.replace(',', "")).ok()
}

impl Matcher {
    pub fn new(cfg: &Config) -> Result<Self> {
        let ignore = cfg
            .ignore_patterns
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("ignore_patterns: {p}")))
            .collect::<Result<_>>()?;
        let mut rules = Vec::new();
        for r in &cfg.rules {
            let Some(kind) = Kind::parse(&r.kind) else {
                bail!("rules: type должен быть expense или income, а не {:?}", r.kind);
            };
            let re = Regex::new(&r.pattern).with_context(|| format!("rules: {}", r.pattern))?;
            if !re.capture_names().flatten().any(|n| n == "amount") {
                bail!("rules: в паттерне нет группы (?P<amount>...): {}", r.pattern);
            }
            rules.push((kind, re));
        }
        let balance = match cfg.balance_pattern.trim() {
            "" => None,
            p => {
                let re = Regex::new(p).with_context(|| format!("balance_pattern: {p}"))?;
                if !re.capture_names().flatten().any(|n| n == "balance") {
                    bail!("balance_pattern: в паттерне нет группы (?P<balance>...): {p}");
                }
                Some(re)
            }
        };
        let date = Regex::new(r"\b(\d{1,2})/(\d{1,2})/(\d{2,4})\b(?:\s+(\d{1,2}):(\d{2})\b)?")?;
        Ok(Self { ignore, rules, date, balance })
    }

    pub fn ignored(&self, text: &str) -> bool {
        self.ignore.iter().any(|re| re.is_match(text))
    }

    pub fn parse(&self, text: &str) -> Option<Parsed> {
        if self.ignored(text) {
            return None;
        }
        for (kind, re) in &self.rules {
            if let Some(c) = re.captures(text) {
                let amount = number(c.name("amount")?.as_str())?;
                if amount <= Decimal::ZERO {
                    return None;
                }
                let merchant = c.name("merchant").map_or("", |m| m.as_str()).trim().to_string();
                let account_hint = c.name("account").map(|m| m.as_str().to_string());
                let balance = self.balance.as_ref().and_then(|re| number(&re.captures(text)?["balance"]));
                return Some(Parsed { kind: *kind, amount, merchant, account_hint, balance });
            }
        }
        None
    }

    /// Дата операции из текста SMS (`21/09/26 10:50 ...`): она точнее даты доставки сообщения,
    /// если Mac спал или SMS переслали с задержкой.
    pub fn text_date(&self, text: &str) -> Option<NaiveDate> {
        let c = self.date.captures(text)?;
        let (d, m, mut y): (u32, u32, i32) = (c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?);
        if y < 100 {
            y += 2000;
        } else if y > 2400 {
            y -= 543; // буддийский год
        }
        NaiveDate::from_ymd_opt(y, m, d)
    }

    /// Дата и время операции из текста SMS (`21/09/26 10:50`) — по часам банка; времени нет — `None`.
    pub fn text_datetime(&self, text: &str) -> Option<NaiveDateTime> {
        let c = self.date.captures(text)?;
        let time = NaiveTime::from_hms_opt(c.get(4)?.as_str().parse().ok()?, c.get(5)?.as_str().parse().ok()?, 0)?;
        Some(self.text_date(text)?.and_time(time))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher() -> Matcher {
        Matcher::new(&Config::default()).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn expense_and_income() {
        let m = matcher();
        let p = m.parse("21/09/26 10:50 A/C X-5019 Withdrawal 60.00 Outstanding Balance 8,195.25 Baht.").unwrap();
        assert_eq!((p.kind, p.amount, p.account_hint.as_deref()), (Kind::Expense, dec("60.00"), Some("5019")));
        let p = m.parse("20/09/26 11:44 A/C X-5019 Deposit 11,088.02 Outstanding Balance 12,282.25 Baht.").unwrap();
        assert_eq!((p.kind, p.amount), (Kind::Income, dec("11088.02")));
    }

    #[test]
    fn transfer_uses_source_account() {
        let p = matcher()
            .parse("20/09/26 21:26 A/C X-5019 transferred 120.00 Baht to A/C X-8842 Outstanding Balance 10,002.25 Baht.")
            .unwrap();
        assert_eq!((p.kind, p.amount, p.account_hint.as_deref()), (Kind::Expense, dec("120.00"), Some("5019")));
    }

    #[test]
    fn account_is_optional() {
        let p = matcher().parse("21/09/26 10:50 Withdrawal 60.00 Outstanding Balance 1.00 Baht.").unwrap();
        assert_eq!((p.amount, p.account_hint), (dec("60.00"), None));
    }

    #[test]
    fn otp_and_unknown_are_skipped() {
        let m = matcher();
        assert_eq!(m.parse("OTP 123456 Withdrawal 100.00"), None);
        assert_eq!(m.parse("Your statement is ready"), None);
    }

    #[test]
    fn rule_without_amount_group_is_rejected() {
        let mut cfg = Config::default();
        cfg.rules[0].pattern = "withdrawal".into();
        assert!(Matcher::new(&cfg).is_err());
    }

    #[test]
    fn balance_from_text() {
        let m = matcher();
        let p = m.parse("25/10/26 14:27 A/C X-6774 Withdrawal 38.20 Outstanding Balance 6,510.29 Baht.").unwrap();
        assert_eq!((p.amount, p.balance), (dec("38.20"), Some(dec("6510.29"))));
        let p = m.parse("A/C X-5019 Deposit 500.00 Avail Bal 1,000.00").unwrap();
        assert_eq!(p.balance, Some(dec("1000.00")));
        assert_eq!(m.parse("21/09/26 10:50 Withdrawal 60.00").unwrap().balance, None);
        let mut cfg = Config { balance_pattern: String::new(), ..Config::default() };
        let p = Matcher::new(&cfg).unwrap().parse("Withdrawal 1.00 Outstanding Balance 5.00 Baht").unwrap();
        assert_eq!(p.balance, None, "пустой паттерн — остаток не ищется");
        cfg.balance_pattern = "balance".into();
        assert!(Matcher::new(&cfg).is_err(), "без группы balance");
    }

    #[test]
    fn date_and_time_from_text() {
        let m = matcher();
        let at = |d: (i32, u32, u32), t: (u32, u32)| NaiveDate::from_ymd_opt(d.0, d.1, d.2).unwrap().and_hms_opt(t.0, t.1, 0);
        assert_eq!(m.text_datetime("25/10/26 14:27 A/C X-6774 Withdrawal 38.20"), at((2026, 10, 25), (14, 27)));
        assert_eq!(m.text_datetime("1/2/2569 9:05 x"), at((2026, 2, 1), (9, 5)));
        assert_eq!(m.text_datetime("25/10/26 A/C X-6774"), None, "без времени");
        assert_eq!(m.text_datetime("25/10/26 25:00 x"), None);
        assert_eq!(m.text_date("25/10/26 14:27 x"), NaiveDate::from_ymd_opt(2026, 10, 25));
    }

    #[test]
    fn date_from_text() {
        let m = matcher();
        let d = |y, mo, da| NaiveDate::from_ymd_opt(y, mo, da);
        assert_eq!(m.text_date("21/09/26 10:50 A/C X-5019 Withdrawal 60.00"), d(2026, 9, 21));
        assert_eq!(m.text_date("1/2/2026 x"), d(2026, 2, 1));
        assert_eq!(m.text_date("01/02/2569 x"), d(2026, 2, 1));
        assert_eq!(m.text_date("31/02/26 x"), None);
        assert_eq!(m.text_date("no date"), None);
    }
}
