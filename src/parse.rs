//! Разбор текста банковского SMS.

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
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
}

pub struct Matcher {
    ignore: Vec<Regex>,
    rules: Vec<(Kind, Regex)>,
    date: Regex,
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
        Ok(Self { ignore, rules, date: Regex::new(r"\b(\d{1,2})/(\d{1,2})/(\d{2,4})\b")? })
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
                let amount = Decimal::from_str(&c.name("amount")?.as_str().replace(',', "")).ok()?;
                if amount <= Decimal::ZERO {
                    return None;
                }
                let merchant = c.name("merchant").map_or("", |m| m.as_str()).trim().to_string();
                let account_hint = c.name("account").map(|m| m.as_str().to_string());
                return Some(Parsed { kind: *kind, amount, merchant, account_hint });
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
            .parse(
                "20/09/26 21:26 A/C X-5019 transferred 120.00 Baht to A/C X-8842 Outstanding Balance 10,002.25 Baht.",
            )
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
