//! Защита от дублей: что уже есть в бухгалтерии для этого SMS.
//!
//! Сравниваем по полям, которые есть и в SMS, и в операции, и которые человек вряд ли изменит:
//! счёт, направление (расход/доход), сумма и дата (с допуском в пару дней). Описание и категорию
//! НЕ сравниваем — при ручном вводе они всегда другие. Каждая внесённая вручную операция «закрывает»
//! только один SMS: два одинаковых платежа за день не склеятся в один.
//!
//! Операции, созданные самим приложением, несут в note позиции метку `[sms:<guid>]`: по ней видно,
//! что SMS уже отправлялся (например, после сброса состояния или `--from-rowid`).

use chrono::NaiveDate;
use regex::Regex;
use rust_decimal::Decimal;
use serde_json::Value;
use std::{collections::HashMap, str::FromStr};

use crate::parse::Kind;

/// Операция из `GET /api/transactions`, только нужные поля.
#[derive(Debug, Clone, PartialEq)]
pub struct Existing {
    pub id: String,
    /// EXPENSE / INCOME / TRANSFER / OPENING
    pub kind: String,
    pub account: String,
    pub to_account: Option<String>,
    pub total: Decimal,
    pub to_amount: Option<Decimal>,
    pub fee: Decimal,
    pub refund: bool,
    pub date: NaiveDate,
    /// guid SMS из метки `[sms:…]`, если операцию создало это приложение
    pub sms_guid: Option<String>,
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s.trim()).ok(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

/// guid SMS в виде, пригодном для метки.
fn clean_guid(guid: &str) -> String {
    guid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Метка SMS в тексте.
pub fn sms_marker(guid: &str) -> String {
    format!("[sms:{}]", clean_guid(guid))
}

impl Existing {
    pub fn from_json(v: &Value) -> Option<Self> {
        let marker = Regex::new(r"\[sms:([^\]\s]+)\]").ok()?;
        let mut texts: Vec<&str> = v["items"].as_array().into_iter().flatten().filter_map(|i| i["note"].as_str()).collect();
        if let Some(d) = v["description"].as_str() {
            texts.push(d);
        }
        let sms_guid = texts.iter().find_map(|t| marker.captures(t)).map(|c| c[1].to_string());
        Some(Self {
            id: v["id"].as_str()?.to_string(),
            kind: v["tx_kind"].as_str()?.to_string(),
            account: v["account_code"].as_str()?.to_uppercase(),
            to_account: v["to_account_code"].as_str().map(str::to_uppercase),
            total: dec(&v["total"])?,
            to_amount: dec(&v["to_amount"]),
            fee: dec(&v["fee_amount"]).unwrap_or_default(),
            refund: v["refund"].as_bool().unwrap_or(false),
            date: NaiveDate::parse_from_str(v["occurred_on"].as_str()?, "%Y-%m-%d").ok()?,
            sms_guid,
        })
    }

    /// Сколько зачислено на счёт назначения (перевод).
    fn credited(&self) -> Decimal {
        self.to_amount.unwrap_or(self.total - self.fee)
    }

    /// Совпадает ли операция с SMS по счёту, направлению и сумме.
    fn matches(&self, p: &Probe) -> bool {
        match self.kind.as_str() {
            // возврат расхода — это деньги на счёт, возврат дохода — деньги со счёта
            "EXPENSE" | "INCOME" => {
                let outflow = (self.kind == "EXPENSE") != self.refund;
                self.account == p.account && outflow == (p.kind == Kind::Expense) && self.total == p.amount
            }
            "TRANSFER" => match p.kind {
                Kind::Expense => self.account == p.account && self.total == p.amount,
                Kind::Income => self.to_account.as_deref() == Some(p.account.as_str()) && self.credited() == p.amount,
            },
            _ => false,
        }
    }
}

/// Что известно про SMS.
pub struct Probe {
    pub guid: String,
    pub kind: Kind,
    pub amount: Decimal,
    pub account: String,
    pub date: NaiveDate,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// SMS уже отправлялся: операция с его меткой есть
    AlreadySent(String),
    /// такая же операция уже внесена вручную
    Entered(String),
    /// дублей нет, можно создавать
    New,
}

/// `claimed`: id ручной операции -> guid SMS, который её уже «занял».
pub fn judge(existing: &[Existing], p: &Probe, claimed: &HashMap<String, String>, tolerance_days: i64, skip_entered: bool) -> Verdict {
    let mine = clean_guid(&p.guid);
    if let Some(e) = existing.iter().find(|e| e.sms_guid.as_deref() == Some(mine.as_str())) {
        return Verdict::AlreadySent(e.id.clone());
    }
    if !skip_entered {
        return Verdict::New;
    }
    existing
        .iter()
        // операции приложения принадлежат своим SMS; «занятые» ручные — тоже (кроме занявшего их SMS:
        // при повторной обработке того же сообщения оно снова узнаёт свою операцию)
        .filter(|e| e.sms_guid.is_none() && claimed.get(&e.id).is_none_or(|g| *g == p.guid))
        .filter(|e| (e.date - p.date).num_days().abs() <= tolerance_days && e.matches(p))
        // ближайшая по дате; при равенстве — стабильно по id
        .min_by_key(|e| ((e.date - p.date).num_days().abs(), e.id.clone()))
        .map_or(Verdict::New, |e| Verdict::Entered(e.id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }
    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn tx(id: &str, kind: &str, account: &str, total: &str, date: &str) -> Existing {
        Existing::from_json(&json!({
            "id": id, "tx_kind": kind, "account_code": account, "total": total,
            "occurred_on": date, "refund": false, "items": [], "description": "кофе"
        }))
        .unwrap()
    }

    fn probe(kind: Kind, amount: &str, account: &str, date: &str) -> Probe {
        Probe { guid: "G-1".into(), kind, amount: d(amount), account: account.into(), date: day(date) }
    }

    fn judge_default(ex: &[Existing], p: &Probe) -> Verdict {
        judge(ex, p, &HashMap::new(), 1, true)
    }

    #[test]
    fn manual_entry_is_a_duplicate() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-21")];
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "60.00", "CARD", "2026-09-21")), Verdict::Entered("a".into()));
    }

    #[test]
    fn date_tolerance_is_respected() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-22")];
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::Entered("a".into()));
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), &HashMap::new(), 0, true), Verdict::New);
        let far = [tx("a", "EXPENSE", "CARD", "60", "2026-09-24")];
        assert_eq!(judge_default(&far, &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::New);
    }

    #[test]
    fn other_amount_account_or_direction_is_not_a_duplicate() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-21")];
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "61", "CARD", "2026-09-21")), Verdict::New);
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "60", "CASH", "2026-09-21")), Verdict::New);
        assert_eq!(judge_default(&ex, &probe(Kind::Income, "60", "CARD", "2026-09-21")), Verdict::New);
    }

    #[test]
    fn one_manual_entry_covers_only_one_sms() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-21")];
        let mut claimed = HashMap::new();
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), &claimed, 1, true), Verdict::Entered("a".into()));
        claimed.insert("a".to_string(), "G-1".to_string());
        // другой SMS на ту же сумму её не займёт...
        let mut other = probe(Kind::Expense, "60", "CARD", "2026-09-21");
        other.guid = "G-2".into();
        assert_eq!(judge(&ex, &other, &claimed, 1, true), Verdict::New);
        // ...а тот же SMS при повторной обработке снова узнает свою операцию, а не создаст дубль
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), &claimed, 1, true), Verdict::Entered("a".into()));
    }

    #[test]
    fn auto_created_entries_belong_to_their_sms() {
        let auto = Existing::from_json(&json!({
            "id": "b", "tx_kind": "EXPENSE", "account_code": "CARD", "total": "60.00",
            "occurred_on": "2026-09-21", "items": [{"note": "[sms:G-1] 21/09/26 Withdrawal 60.00"}]
        }))
        .unwrap();
        assert_eq!(auto.sms_guid.as_deref(), Some("G-1"));
        // тот же SMS ещё раз -> уже отправлен
        assert_eq!(judge_default(&[auto.clone()], &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::AlreadySent("b".into()));
        // другой SMS на ту же сумму -> автоматическая операция его не закрывает
        let mut other = probe(Kind::Expense, "60", "CARD", "2026-09-21");
        other.guid = "G-2".into();
        assert_eq!(judge_default(&[auto], &other), Verdict::New);
    }

    #[test]
    fn transfers_count_on_both_sides() {
        let t = Existing::from_json(&json!({
            "id": "t", "tx_kind": "TRANSFER", "account_code": "CARD", "to_account_code": "CASH",
            "total": "120.00", "fee_amount": "20.00", "occurred_on": "2026-09-20", "items": []
        }))
        .unwrap();
        assert_eq!(judge_default(&[t.clone()], &probe(Kind::Expense, "120", "CARD", "2026-09-20")), Verdict::Entered("t".into()));
        assert_eq!(judge_default(&[t.clone()], &probe(Kind::Income, "100", "CASH", "2026-09-20")), Verdict::Entered("t".into()));
        assert_eq!(judge_default(&[t], &probe(Kind::Income, "120", "CASH", "2026-09-20")), Verdict::New);
    }

    #[test]
    fn refund_flips_direction() {
        let mut r = tx("r", "EXPENSE", "CARD", "60", "2026-09-21");
        r.refund = true;
        assert_eq!(judge_default(&[r.clone()], &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::New);
        assert_eq!(judge_default(&[r], &probe(Kind::Income, "60", "CARD", "2026-09-21")), Verdict::Entered("r".into()));
    }

    #[test]
    fn check_can_be_turned_off() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-21")];
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), &HashMap::new(), 1, false), Verdict::New);
    }

    #[test]
    fn closest_date_wins() {
        let ex = [tx("far", "EXPENSE", "CARD", "60", "2026-09-22"), tx("near", "EXPENSE", "CARD", "60", "2026-09-21")];
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::Entered("near".into()));
    }

    #[test]
    fn marker_is_sanitized() {
        assert_eq!(sms_marker("p:0/AB-12 x"), "[sms:p_0_AB-12_x]");
    }
}
