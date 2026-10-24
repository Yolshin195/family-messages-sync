//! Логи без бесконечного роста: файл больше `MAX_BYTES` переименовывается в `<имя>.1` (прежний `.1` удаляется),
//! поэтому на каждый лог на диске не больше двух файлов по мегабайту.
//!
//! Логи приложения (`unparsed.log`, `failed.log`) проверяются перед каждой записью. Логи launchd (`out.log`,
//! `err.log`) — в начале `run`: launchd открывает их заново при каждом запуске, так что текущий запуск допишет
//! вывод в `.1`, а следующий начнёт новый файл.

use std::{
    ffi::OsString,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use crate::config::{NAME, home};

/// Размер, после которого лог уходит в `.1`.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// Архив лога: `failed.log` → `failed.log.1`.
pub fn archive(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".1");
    PathBuf::from(name)
}

/// Переименовать лог в `.1`, если он больше `max` байт. `true` — переименован.
pub fn rotate(path: &Path, max: u64) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(m) if m.len() > max => fs::rename(path, archive(path)).map(|_| true),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Дописать строку в лог приложения.
pub fn append(path: &Path, line: &str) -> io::Result<()> {
    rotate(path, MAX_BYTES)?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

/// Каталог логов launchd: `FAMILY_SYNC_LOG_DIR` из plist (`make agent LOG_DIR=…`), иначе ~/Library/Logs/<NAME>.
fn launchd_dir() -> PathBuf {
    std::env::var_os("FAMILY_SYNC_LOG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join("Library/Logs").join(NAME))
}

/// `out.log` и `err.log` launchd. Ошибка не останавливает импорт: она уходит в err.log.
pub fn rotate_launchd() {
    let dir = launchd_dir();
    for file in ["out.log", "err.log"] {
        if let Err(e) = rotate(&dir.join(file), MAX_BYTES) {
            eprintln!("не переименовать {} в .1: {e}", dir.join(file).display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{NAME}-logs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn big_log_goes_to_archive_and_replaces_old_one() {
        let d = dir("rotate");
        let log = d.join("err.log");
        fs::write(archive(&log), "старый архив").unwrap();
        fs::write(&log, "12345").unwrap();
        assert!(!rotate(&log, 5).unwrap(), "не больше лимита — остаётся");
        fs::write(&log, "123456").unwrap();
        assert!(rotate(&log, 5).unwrap());
        assert!(!log.exists());
        assert_eq!(fs::read_to_string(d.join("err.log.1")).unwrap(), "123456");
        assert!(!rotate(&log, 5).unwrap(), "лога нет — нечего переименовывать");
    }

    #[test]
    fn append_keeps_log_under_limit() {
        let d = dir("append");
        let log = d.join("failed.log");
        fs::write(&log, vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        append(&log, "новая строка").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "новая строка\n");
        assert_eq!(fs::metadata(archive(&log)).unwrap().len(), MAX_BYTES + 1);
        append(&log, "ещё").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "новая строка\nещё\n");
    }
}
