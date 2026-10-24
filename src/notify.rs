//! Уведомления macOS и правила, когда их показывать.

use chrono::{DateTime, Duration, Local};
use std::process::Command;

use crate::config::{Alert, State};

/// Заголовок всех уведомлений.
pub const TITLE: &str = "Семейная бухгалтерия";

/// Одно и то же уведомление об остановке — не чаще раза в столько часов.
const ALERT_HOURS: i64 = 6;

pub trait Notifier {
    fn notify(&self, title: &str, text: &str);
}

/// Центр уведомлений macOS (`osascript`).
pub struct MacNotifier;

/// Строка AppleScript в кавычках.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " "))
}

impl Notifier for MacNotifier {
    fn notify(&self, title: &str, text: &str) {
        let script = format!("display notification {} with title {}", quote(text), quote(title));
        // без уведомления обработка продолжается, ошибка уходит в err.log
        match Command::new("osascript").args(["-e", &script]).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => eprintln!("уведомление не показано: osascript: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => eprintln!("уведомление не показано: osascript: {e}"),
        }
    }
}

/// Текст в одну строку, не длиннее `max` символов.
pub fn short(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Импорт остановлен. Уведомляет, если это новость: та же причина, пока импорт так и стоит,
/// повторяется не чаще раза в 6 часов.
pub fn stopped(state: &mut State, why: &str, now: DateTime<Local>, n: &dyn Notifier) {
    let reason: String = why.chars().take(40).collect();
    let known = state
        .alert
        .as_ref()
        .is_some_and(|a| a.stopped && a.reason == reason && now - a.at < Duration::hours(ALERT_HOURS));
    if !known {
        let why = short(why, 150);
        n.notify(TITLE, &format!("Импорт SMS остановлен: {}. Подробности: make logs", why.trim_end_matches('.')));
        state.alert = Some(Alert { reason, at: now, stopped: true });
    }
}

/// Запуск без остановки: если до этого импорт стоял, сообщить, что он снова работает.
pub fn recovered(state: &mut State, n: &dyn Notifier) {
    if let Some(a) = state.alert.as_mut().filter(|a| a.stopped) {
        n.notify(TITLE, "Импорт SMS снова работает");
        a.stopped = false;
    }
}

/// Уведомления в списке, для тестов.
#[cfg(test)]
#[derive(Default)]
pub struct Recorded(pub std::cell::RefCell<Vec<String>>);

#[cfg(test)]
impl Recorded {
    pub fn texts(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

#[cfg(test)]
impl Notifier for Recorded {
    fn notify(&self, title: &str, text: &str) {
        assert_eq!(title, TITLE);
        self.0.borrow_mut().push(text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 23, h, 10, 0).unwrap()
    }

    const NET: &str = "запрос /api/transactions (проверьте api_url и доступность бухгалтерии): error sending request";

    #[test]
    fn same_reason_is_not_repeated_within_six_hours() {
        let (mut st, n) = (State::default(), Recorded::default());
        stopped(&mut st, NET, t(9), &n);
        stopped(&mut st, &format!("{NET} (другой хвост)"), t(10), &n);
        assert_eq!(n.texts().len(), 1);
        assert!(n.texts()[0].starts_with("Импорт SMS остановлен: запрос /api/transactions"), "{:?}", n.texts());
        assert!(n.texts()[0].ends_with(". Подробности: make logs"));
        // через 6 часов — напоминание
        stopped(&mut st, NET, t(15), &n);
        assert_eq!(n.texts().len(), 2);
    }

    #[test]
    fn other_reason_is_news() {
        let (mut st, n) = (State::default(), Recorded::default());
        stopped(&mut st, NET, t(9), &n);
        stopped(&mut st, "/api/transactions: 401 токен недействителен, истёк или отозван", t(10), &n);
        assert_eq!(n.texts().len(), 2);
        assert_eq!(st.alert.as_ref().map(|a| a.reason.as_str()), Some("/api/transactions: 401 токен недействите"));
    }

    #[test]
    fn recovery_is_announced_once() {
        let (mut st, n) = (State::default(), Recorded::default());
        recovered(&mut st, &n);
        assert!(n.texts().is_empty());
        stopped(&mut st, NET, t(9), &n);
        recovered(&mut st, &n);
        recovered(&mut st, &n);
        assert_eq!(n.texts()[1..], ["Импорт SMS снова работает".to_string()]);
        assert_eq!(st.alert.as_ref().map(|a| a.stopped), Some(false));
        // после восстановления новая остановка — снова новость, даже с той же причиной
        stopped(&mut st, NET, t(10), &n);
        assert_eq!(n.texts().len(), 3);
    }

    #[test]
    fn applescript_quoting() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(quote("a\nb"), "\"a b\"");
    }

    #[test]
    fn short_text() {
        assert_eq!(short("  a\n b  ", 10), "a b");
        assert_eq!(short("Your statement is ready for download", 24), "Your statement is ready…");
    }
}
