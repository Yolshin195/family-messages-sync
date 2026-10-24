//! Защита от дублей: что уже есть в бухгалтерии для этого SMS.
//!
//! Какие SMS уже заведены, бухгалтерия помнит сама: у операции есть связи `external_refs` с guid
//! сообщений. По ним видно, что SMS уже отправлялось (повтор, второй Mac, сброс состояния), и что
//! ручную операцию уже подтвердило другое SMS: два одинаковых платежа за день не склеятся в один.
//!
//! Ручную операцию ищем по полям, которые есть и в SMS, и в операции, и которые человек вряд ли изменит:
//! счёт, направление (расход/доход), сумма и дата (с допуском в пару дней). Описание и категорию
//! НЕ сравниваем — при ручном вводе они всегда другие.
//!
//! Операции, созданные до перехода на связи, несут в note позиции метку `[sms:<guid>]`: её тоже читаем.

use chrono::NaiveDate;
use regex::Regex;
use rust_decimal::Decimal;
use serde_json::Value;
use std::{str::FromStr, sync::LazyLock};

use crate::parse::Kind;

/// Старая метка SMS в заметке позиции или описании.
static MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[sms:([^\]\s]+)\]").expect("regex метки"));

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
    /// guid SMS, связанных с операцией: `external_refs` нашего `provider` и старые метки `[sms:…]`
    pub sms_guids: Vec<String>,
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s.trim()).ok(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

/// guid SMS в том виде, в каком его записывала старая метка.
fn clean_guid(guid: &str) -> String {
    guid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

impl Existing {
    /// Связи с другим `provider` не учитываются.
    pub fn from_json(v: &Value, provider: &str) -> Option<Self> {
        let refs = v["external_refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["provider"].as_str() == Some(provider))
            .filter_map(|r| r["external_id"].as_str().map(str::to_string));
        let notes = v["items"].as_array().into_iter().flatten().filter_map(|i| i["note"].as_str());
        let marks =
            notes.chain(v["description"].as_str()).flat_map(|t| MARKER.captures_iter(t)).map(|c| c[1].to_string());
        let mut sms_guids: Vec<String> = Vec::new();
        for g in refs.chain(marks) {
            if !sms_guids.contains(&g) {
                sms_guids.push(g);
            }
        }
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
            sms_guids,
        })
    }

    /// Связано ли с операцией это SMS.
    pub fn has_sms(&self, guid: &str) -> bool {
        let old = clean_guid(guid);
        self.sms_guids.iter().any(|g| g == guid || *g == old)
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

    /// Может ли SMS подтвердить операцию: её ещё не подтвердило ни одно SMS. Перевод подтверждают два SMS —
    /// списание и зачисление, поэтому он занят, только если SMS той же стороны уже есть
    /// (сторона прикреплённого SMS неизвестна — считаем, что та же).
    fn free_for(&self, p: &Probe, side: impl Fn(&str) -> Option<Kind>) -> bool {
        if self.kind == "TRANSFER" {
            !self.sms_guids.iter().any(|g| side(g).is_none_or(|k| k == p.kind))
        } else {
            self.sms_guids.is_empty()
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
    /// SMS уже отправлялось: связано с этой операцией
    AlreadySent(String),
    /// такие же операции внесены вручную и не подтверждены другим SMS; ближайшие по дате первыми
    Entered(Vec<String>),
    /// дублей нет, можно создавать
    New,
}

/// `side` — направление уже прикреплённого SMS по его guid (из журнала), если известно.
pub fn judge(
    existing: &[Existing],
    p: &Probe,
    tolerance_days: i64,
    skip_entered: bool,
    side: impl Fn(&str) -> Option<Kind>,
) -> Verdict {
    if let Some(e) = existing.iter().find(|e| e.has_sms(&p.guid)) {
        return Verdict::AlreadySent(e.id.clone());
    }
    if !skip_entered {
        return Verdict::New;
    }
    let gap = |e: &Existing| (e.date - p.date).num_days().abs();
    let mut found: Vec<&Existing> =
        existing.iter().filter(|e| gap(e) <= tolerance_days && e.matches(p) && e.free_for(p, &side)).collect();
    // ближайшая по дате; при равенстве — стабильно по id
    found.sort_by(|a, b| (gap(a), &a.id).cmp(&(gap(b), &b.id)));
    if found.is_empty() { Verdict::New } else { Verdict::Entered(found.into_iter().map(|e| e.id.clone()).collect()) }
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
        Existing::from_json(
            &json!({
                "id": id, "tx_kind": kind, "account_code": account, "total": total,
                "occurred_on": date, "refund": false, "items": [], "description": "кофе", "external_refs": []
            }),
            "imessage",
        )
        .unwrap()
    }

    fn linked(mut e: Existing, guid: &str) -> Existing {
        e.sms_guids.push(guid.into());
        e
    }

    fn probe(kind: Kind, amount: &str, account: &str, date: &str) -> Probe {
        Probe { guid: "G-1".into(), kind, amount: d(amount), account: account.into(), date: day(date) }
    }

    fn unknown(_: &str) -> Option<Kind> {
        None
    }

    fn judge_default(ex: &[Existing], p: &Probe) -> Verdict {
        judge(ex, p, 1, true, unknown)
    }

    fn entered(ids: &[&str]) -> Verdict {
        Verdict::Entered(ids.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn refs_and_old_markers_give_sms_guids() {
        let e = Existing::from_json(
            &json!({
                "id": "b", "tx_kind": "EXPENSE", "account_code": "CARD", "total": "60.00", "occurred_on": "2026-09-21",
                "items": [{"note": "[sms:G-2] [АВТО: проверить] SMS: 21/09/26 Withdrawal 60.00"}],
                "external_refs": [
                    {"provider": "imessage", "external_id": "G-1", "transaction_id": "b", "link": "CREATED"},
                    {"provider": "other", "external_id": "X-1", "transaction_id": "b", "link": "CREATED"}
                ]
            }),
            "imessage",
        )
        .unwrap();
        assert_eq!(e.sms_guids, ["G-1", "G-2"]);
    }

    #[test]
    fn linked_sms_is_already_sent() {
        let ex = [linked(tx("b", "EXPENSE", "CARD", "60", "2026-09-21"), "G-1")];
        assert_eq!(
            judge_default(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21")),
            Verdict::AlreadySent("b".into())
        );
        // старая метка хранит guid очищенным
        let old = [linked(tx("b", "EXPENSE", "CARD", "60", "2026-09-21"), "p_0_AB-12")];
        let mut p = probe(Kind::Expense, "60", "CARD", "2026-09-21");
        p.guid = "p:0/AB-12".into();
        assert_eq!(judge_default(&old, &p), Verdict::AlreadySent("b".into()));
        // даже если пару искать не надо
        assert_eq!(
            judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), 1, false, unknown),
            Verdict::AlreadySent("b".into())
        );
    }

    #[test]
    fn manual_entry_confirmed_by_other_sms_is_not_a_candidate() {
        let free = tx("a", "EXPENSE", "CARD", "60", "2026-09-21");
        assert_eq!(
            judge_default(std::slice::from_ref(&free), &probe(Kind::Expense, "60.00", "CARD", "2026-09-21")),
            entered(&["a"])
        );
        let taken = linked(free, "G-2");
        assert_eq!(judge_default(&[taken], &probe(Kind::Expense, "60", "CARD", "2026-09-21")), Verdict::New);
    }

    #[test]
    fn transfer_takes_one_sms_per_side() {
        let t = Existing::from_json(
            &json!({
                "id": "t", "tx_kind": "TRANSFER", "account_code": "CARD", "to_account_code": "SAVE",
                "total": "120.00", "occurred_on": "2026-09-20", "items": [],
                "external_refs": [{"provider": "imessage", "external_id": "G-out"}]
            }),
            "imessage",
        )
        .unwrap();
        let side = |g: &str| (g == "G-out").then_some(Kind::Expense);
        // списание уже прикреплено: зачисление — кандидат
        assert_eq!(
            judge(std::slice::from_ref(&t), &probe(Kind::Income, "120", "SAVE", "2026-09-20"), 1, true, side),
            entered(&["t"])
        );
        // второе списание на ту же сумму — нет
        assert_eq!(
            judge(std::slice::from_ref(&t), &probe(Kind::Expense, "120", "CARD", "2026-09-20"), 1, true, side),
            Verdict::New
        );
        // сторона прикреплённого SMS неизвестна — считаем, что та же
        assert_eq!(
            judge(std::slice::from_ref(&t), &probe(Kind::Income, "120", "SAVE", "2026-09-20"), 1, true, unknown),
            Verdict::New
        );
        // обе стороны прикреплены
        let both = linked(t, "G-in");
        let side = |g: &str| Some(if g == "G-out" { Kind::Expense } else { Kind::Income });
        assert_eq!(judge(&[both], &probe(Kind::Income, "120", "SAVE", "2026-09-20"), 1, true, side), Verdict::New);
    }

    #[test]
    fn date_tolerance_is_respected() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-22")];
        assert_eq!(judge_default(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21")), entered(&["a"]));
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), 0, true, unknown), Verdict::New);
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
    fn transfers_count_on_both_sides() {
        let t = Existing::from_json(
            &json!({
                "id": "t", "tx_kind": "TRANSFER", "account_code": "CARD", "to_account_code": "CASH",
                "total": "120.00", "fee_amount": "20.00", "occurred_on": "2026-09-20", "items": []
            }),
            "imessage",
        )
        .unwrap();
        assert_eq!(
            judge_default(std::slice::from_ref(&t), &probe(Kind::Expense, "120", "CARD", "2026-09-20")),
            entered(&["t"])
        );
        assert_eq!(
            judge_default(std::slice::from_ref(&t), &probe(Kind::Income, "100", "CASH", "2026-09-20")),
            entered(&["t"])
        );
        assert_eq!(judge_default(&[t], &probe(Kind::Income, "120", "CASH", "2026-09-20")), Verdict::New);
    }

    #[test]
    fn refund_flips_direction() {
        let mut r = tx("r", "EXPENSE", "CARD", "60", "2026-09-21");
        r.refund = true;
        assert_eq!(
            judge_default(std::slice::from_ref(&r), &probe(Kind::Expense, "60", "CARD", "2026-09-21")),
            Verdict::New
        );
        assert_eq!(judge_default(&[r], &probe(Kind::Income, "60", "CARD", "2026-09-21")), entered(&["r"]));
    }

    #[test]
    fn check_can_be_turned_off() {
        let ex = [tx("a", "EXPENSE", "CARD", "60", "2026-09-21")];
        assert_eq!(judge(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21"), 1, false, unknown), Verdict::New);
    }

    #[test]
    fn closest_date_first_then_id() {
        let ex = [
            tx("far", "EXPENSE", "CARD", "60", "2026-09-22"),
            tx("near-b", "EXPENSE", "CARD", "60", "2026-09-21"),
            tx("near-a", "EXPENSE", "CARD", "60", "2026-09-21"),
        ];
        assert_eq!(
            judge_default(&ex, &probe(Kind::Expense, "60", "CARD", "2026-09-21")),
            entered(&["near-a", "near-b", "far"])
        );
    }
}
