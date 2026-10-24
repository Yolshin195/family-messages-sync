//! Чтение сообщений из Messages (macOS), только чтение chat.db.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::config::home;

pub struct Msg {
    pub rowid: i64,
    pub guid: String,
    pub text: String,
    pub date: DateTime<Local>,
    pub handle: String,
    pub bank: bool,
}

/// На новых macOS текст лежит в attributedBody (typedstream), а не в message.text.
pub fn text_from_attributed(blob: &[u8]) -> Option<String> {
    const MARK: &[u8] = b"NSString";
    let i = blob.windows(MARK.len()).position(|w| w == MARK)?;
    let rest = blob.get(i + MARK.len() + 5..)?;
    let (len, start) = match *rest.first()? {
        0x81 => (u16::from_le_bytes(rest.get(1..3)?.try_into().ok()?) as usize, 3),
        0x82 => (u32::from_le_bytes(rest.get(1..5)?.try_into().ok()?) as usize, 5),
        n => (n as usize, 1),
    };
    let bytes = rest.get(start..start + len)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn apple_date(d: i64) -> DateTime<Local> {
    let secs = if d > 1_000_000_000_000 { d / 1_000_000_000 } else { d };
    DateTime::<Utc>::from_timestamp(secs + 978_307_200, 0).unwrap_or_default().with_timezone(&Local)
}

pub fn open_db() -> Result<Connection> {
    // FAMILY_SYNC_CHAT_DB — путь к копии chat.db для проверки без macOS
    let path = std::env::var_os("FAMILY_SYNC_CHAT_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home().join("Library/Messages/chat.db"));
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| {
        format!(
            "Не открыть {} — выдайте Full Disk Access этому бинарнику: {}",
            path.display(),
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })
}

pub fn max_rowid(db: &Connection) -> Result<i64> {
    Ok(db.query_row("SELECT COALESCE(MAX(ROWID),0) FROM message", [], |r| r.get(0))?)
}

/// Сообщения новее `after` и, дополнительно, сообщения с перечисленными `retry` ROWID, по возрастанию ROWID.
pub fn fetch_messages(db: &Connection, after: i64, retry: &[i64], senders: &[String]) -> Result<Vec<Msg>> {
    let senders: Vec<String> = senders.iter().map(|s| s.to_lowercase()).collect();
    // ROWID — целые числа, подставлять их в запрос безопасно
    let retry_sql = retry.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT m.ROWID, m.guid, m.text, m.attributedBody, m.date, h.id \
         FROM message m JOIN handle h ON m.handle_id = h.ROWID \
         WHERE (m.ROWID > ?1 OR m.ROWID IN ({retry_sql})) AND m.is_from_me = 0 ORDER BY m.ROWID"
    );
    let mut stmt = db.prepare(&sql).context("чтение chat.db (проверьте Full Disk Access)")?;
    let rows = stmt.query_map([after], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<Vec<u8>>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (rowid, guid, text, attr, date, handle) = row?;
        let handle = handle.unwrap_or_default();
        let text = text
            .filter(|t| !t.is_empty())
            .or_else(|| attr.as_deref().and_then(text_from_attributed))
            .unwrap_or_default();
        let bank = senders.iter().any(|s| handle.to_lowercase().contains(s));
        let guid = guid.filter(|g| !g.is_empty()).unwrap_or_else(|| format!("rowid-{rowid}"));
        out.push(Msg { rowid, guid, text, date: apple_date(date), handle, bank });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributed_body() {
        // "NSString" + 5 байт преамбулы + длина + текст
        let mut blob = b"xx NSString".to_vec();
        blob.extend([0x01, 0x94, 0x84, 0x01, 0x2b, 5]);
        blob.extend(b"hello");
        assert_eq!(text_from_attributed(&blob).as_deref(), Some("hello"));
    }
}
