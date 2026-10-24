//! API-токен в Keychain (через /usr/bin/security). Для тестов и не-macOS — переменная FAMILY_SYNC_TOKEN.

use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::config::{KEYCHAIN_ACCOUNT, NAME};

pub fn get(service: &str) -> Result<String> {
    if let Some(t) = std::env::var("FAMILY_SYNC_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(t);
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", KEYCHAIN_ACCOUNT, "-w"])
        .output()
        .context("запуск security")?;
    if !out.status.success() {
        bail!("API-токен не найден в Keychain. Выполните: {NAME} set-token");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn exists(service: &str) -> bool {
    get(service).is_ok()
}

pub fn set(service: &str) -> Result<()> {
    // `-w` последним аргументом без значения -> security сам спросит значение (ввод скрыт)
    let st = Command::new("security")
        .args(["add-generic-password", "-U", "-s", service, "-a", KEYCHAIN_ACCOUNT, "-w"])
        .status()
        .context("запуск security")?;
    if !st.success() {
        bail!("не удалось сохранить токен в Keychain");
    }
    Ok(())
}

pub fn delete(service: &str) {
    let _ = Command::new("security").args(["delete-generic-password", "-s", service, "-a", KEYCHAIN_ACCOUNT]).output();
}
