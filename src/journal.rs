//! Журнал сообщений `app.db`: что стало с каждым сообщением от банка (команды `log`, `show`, `status`).

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, SecondsFormat};
use clap::ValueEnum;
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use std::{path::Path, time::Duration};

use crate::parse::Kind;

/// Итог обработки сообщения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum Status {
    /// создана новая операция
    Created,
    /// SMS прикреплено к уже внесённой операции
    Matched,
    /// операция по этому SMS уже была
    AlreadySent,
    /// операцию по этому SMS удалили в бухгалтерии
    Deleted,
    /// бухгалтерия отказала или не определён счёт: сообщение в очереди повторов
    Rejected,
    /// от банка, но ни одно правило не подошло
    Unrecognized,
    /// совпало с ignore_patterns (коды подтверждения)
    Ignored,
    /// старое сообщение (подкачка истории)
    Old,
}

impl Status {
    pub fn code(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Matched => "MATCHED",
            Self::AlreadySent => "ALREADY_SENT",
            Self::Deleted => "DELETED",
            Self::Rejected => "REJECTED",
            Self::Unrecognized => "UNRECOGNIZED",
            Self::Ignored => "IGNORED",
            Self::Old => "OLD",
        }
    }

    fn from_code(s: &str) -> Option<Self> {
        Self::value_variants().iter().copied().find(|st| st.code() == s)
    }

    /// Подпись в `log`, `show` и выводе `run`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "создано",
            Self::Matched => "найдена ручная",
            Self::AlreadySent => "уже было",
            Self::Deleted => "удалена в бухгалтерии",
            Self::Rejected => "отклонено",
            Self::Unrecognized => "не распознано",
            Self::Ignored => "пропущено (код)",
            Self::Old => "старое",
        }
    }
}

/// Строка журнала.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// ROWID из chat.db
    pub rowid: i64,
    pub guid: String,
    /// handle.id отправителя
    pub sender: String,
    pub received_at: DateTime<Local>,
    /// текст SMS; пусто у IGNORED (коды не храним)
    pub text: String,
    pub status: Status,
    /// текст отказа или пропуска
    pub reason: Option<String>,
    pub kind: Option<Kind>,
    /// десятичная строка, как в SMS: "60.00"
    pub amount: Option<String>,
    pub account_code: Option<String>,
    pub occurred_on: Option<NaiveDate>,
    /// UUID операции в бухгалтерии
    pub transaction_id: Option<String>,
    pub processed_at: DateTime<Local>,
}

pub struct Journal(Connection);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    rowid          INTEGER PRIMARY KEY,
    guid           TEXT    NOT NULL,
    sender         TEXT    NOT NULL,
    received_at    TEXT    NOT NULL,
    text           TEXT    NOT NULL,
    status         TEXT    NOT NULL CHECK (status IN
                     ('CREATED','MATCHED','ALREADY_SENT','DELETED','REJECTED','UNRECOGNIZED','IGNORED','OLD')),
    reason         TEXT    NULL,
    kind           TEXT    NULL,
    amount         TEXT    NULL,
    account_code   TEXT    NULL,
    occurred_on    TEXT    NULL,
    transaction_id TEXT    NULL,
    processed_at   TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_status ON messages (status, received_at);
CREATE INDEX IF NOT EXISTS messages_guid ON messages (guid);
PRAGMA user_version = 1;
";

const COLUMNS: &str = "rowid, guid, sender, received_at, text, status, reason, kind, amount, account_code, occurred_on, transaction_id, processed_at";

/// Время в журнале: RFC 3339 до секунд, местное время Mac. Строки одного вида сравниваются как время.
fn ts(t: DateTime<Local>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn bad_value(col: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(col, Type::Text, Box::new(e))
}

fn entry(r: &Row) -> rusqlite::Result<Entry> {
    let time = |col: usize| -> rusqlite::Result<DateTime<Local>> {
        let s: String = r.get(col)?;
        DateTime::parse_from_rfc3339(&s).map(|t| t.with_timezone(&Local)).map_err(|e| bad_value(col, e))
    };
    let status: String = r.get(5)?;
    let occurred_on: Option<String> = r.get(10)?;
    Ok(Entry {
        rowid: r.get(0)?,
        guid: r.get(1)?,
        sender: r.get(2)?,
        received_at: time(3)?,
        text: r.get(4)?,
        status: Status::from_code(&status).ok_or_else(|| bad_value(5, std::fmt::Error))?,
        reason: r.get(6)?,
        kind: r.get::<_, Option<String>>(7)?.as_deref().and_then(Kind::from_tx_kind),
        amount: r.get(8)?,
        account_code: r.get(9)?,
        occurred_on: occurred_on
            .map(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d"))
            .transpose()
            .map_err(|e| bad_value(10, e))?,
        transaction_id: r.get(11)?,
        processed_at: time(12)?,
    })
}

impl Journal {
    /// `dir/app.db`; схема создаётся, если её нет.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("app.db");
        let db = Connection::open(&path).with_context(|| format!("не открыть журнал {}", path.display()))?;
        Self::init(db).with_context(|| format!("журнал {}", path.display()))
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn init(db: Connection) -> Result<Self> {
        // `run` пишет, пока `status` или `log` читают
        db.busy_timeout(Duration::from_secs(10))?;
        db.execute_batch(SCHEMA)?;
        Ok(Self(db))
    }

    /// Записать итог; повторная обработка того же сообщения перезаписывает строку.
    pub fn write(&self, e: &Entry) -> Result<()> {
        self.0.execute(
            &format!(
                "INSERT INTO messages ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(rowid) DO UPDATE SET guid = excluded.guid, sender = excluded.sender, \
                 received_at = excluded.received_at, text = excluded.text, status = excluded.status, \
                 reason = excluded.reason, kind = excluded.kind, amount = excluded.amount, \
                 account_code = excluded.account_code, occurred_on = excluded.occurred_on, \
                 transaction_id = excluded.transaction_id, processed_at = excluded.processed_at"
            ),
            params![
                e.rowid,
                e.guid,
                e.sender,
                ts(e.received_at),
                e.text,
                e.status.code(),
                e.reason,
                e.kind.map(Kind::tx_kind),
                e.amount,
                e.account_code,
                e.occurred_on.map(|d| d.to_string()),
                e.transaction_id,
                ts(e.processed_at),
            ],
        )?;
        Ok(())
    }

    /// Удалить записи о сообщениях, полученных раньше `before`.
    pub fn purge(&self, before: DateTime<Local>) -> Result<usize> {
        Ok(self.0.execute("DELETE FROM messages WHERE received_at < ?1", [ts(before)])?)
    }

    /// Последние `n` записей по времени получения, в хронологическом порядке.
    pub fn last(&self, n: usize, status: Option<Status>) -> Result<Vec<Entry>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {COLUMNS} FROM messages WHERE ?1 IS NULL OR status = ?1 ORDER BY received_at DESC, rowid DESC LIMIT ?2"
        ))?;
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let mut out =
            stmt.query_map(params![status.map(Status::code), limit], entry)?.collect::<rusqlite::Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    pub fn get(&self, rowid: i64) -> Result<Option<Entry>> {
        Ok(self.0.query_row(&format!("SELECT {COLUMNS} FROM messages WHERE rowid = ?1"), [rowid], entry).optional()?)
    }

    /// Направление уже обработанного SMS (списание или зачисление), если оно есть в журнале.
    pub fn kind_of(&self, guid: &str) -> Result<Option<Kind>> {
        let kind: Option<String> = self
            .0
            .query_row(
                "SELECT kind FROM messages WHERE guid = ?1 AND kind IS NOT NULL ORDER BY processed_at DESC LIMIT 1",
                [guid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(kind.as_deref().and_then(Kind::from_tx_kind))
    }

    /// Сколько сообщений с каждым итогом получено начиная с `since`.
    pub fn counts_since(&self, since: DateTime<Local>) -> Result<Vec<(Status, i64)>> {
        let mut stmt =
            self.0.prepare("SELECT status, count(*) FROM messages WHERE received_at >= ?1 GROUP BY status")?;
        let rows = stmt.query_map([ts(since)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (code, n) = row?;
            if let Some(st) = Status::from_code(&code) {
                out.push((st, n));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as Days, TimeZone};

    fn at(day: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, day, 10, 51, 7).unwrap()
    }

    fn e(rowid: i64, status: Status) -> Entry {
        Entry {
            rowid,
            guid: format!("G-{rowid}"),
            sender: "KBank".into(),
            received_at: at(21),
            text: "21/09/26 10:50 A/C X-5019 Withdrawal 60.00".into(),
            status,
            reason: None,
            kind: Some(Kind::Expense),
            amount: Some("60.00".into()),
            account_code: Some("CARD_KBANK".into()),
            occurred_on: NaiveDate::from_ymd_opt(2026, 9, 21),
            transaction_id: None,
            processed_at: at(21),
        }
    }

    #[test]
    fn every_status_round_trips() {
        let j = Journal::in_memory();
        for (i, st) in Status::value_variants().iter().enumerate() {
            let mut x = e(i as i64 + 1, *st);
            x.reason = Some("почему".into());
            j.write(&x).unwrap();
            assert_eq!(j.get(x.rowid).unwrap(), Some(x));
        }
        assert_eq!(j.last(100, None).unwrap().len(), 8);
        assert_eq!(j.last(100, Some(Status::Old)).unwrap().len(), 1);
    }

    #[test]
    fn rewrite_keeps_one_row_per_rowid() {
        let j = Journal::in_memory();
        let mut x = e(7, Status::Rejected);
        x.reason = Some("422 категория AUTO_EXPENSE не найдена".into());
        j.write(&x).unwrap();
        j.write(&x).unwrap();
        let ok = Entry { status: Status::Created, reason: None, transaction_id: Some("t1".into()), ..x };
        j.write(&ok).unwrap();
        assert_eq!(j.last(100, None).unwrap(), vec![ok]);
        assert!(j.last(100, Some(Status::Rejected)).unwrap().is_empty());
    }

    #[test]
    fn purge_and_counts_use_received_at() {
        let j = Journal::in_memory();
        let now = at(21) + Days::days(90);
        j.write(&Entry { received_at: now - Days::days(100), ..e(1, Status::Created) }).unwrap();
        j.write(&Entry { received_at: now - Days::days(10), ..e(2, Status::Created) }).unwrap();
        j.write(&Entry { received_at: now - Days::days(1), ..e(3, Status::Matched) }).unwrap();
        assert_eq!(j.purge(now - Days::days(90)).unwrap(), 1);
        assert_eq!(j.last(100, None).unwrap().iter().map(|x| x.rowid).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(j.counts_since(now - Days::days(7)).unwrap(), [(Status::Matched, 1)]);
    }

    #[test]
    fn last_takes_newest_and_shows_them_in_order() {
        let j = Journal::in_memory();
        for i in 1..=5 {
            j.write(&Entry { received_at: at(i), ..e(i64::from(i), Status::Created) }).unwrap();
        }
        assert_eq!(j.last(2, None).unwrap().iter().map(|x| x.rowid).collect::<Vec<_>>(), [4, 5]);
    }

    #[test]
    fn kind_of_known_sms() {
        let j = Journal::in_memory();
        j.write(&Entry { kind: Some(Kind::Income), ..e(1, Status::Matched) }).unwrap();
        assert_eq!(j.kind_of("G-1").unwrap(), Some(Kind::Income));
        assert_eq!(j.kind_of("G-2").unwrap(), None);
    }
}
