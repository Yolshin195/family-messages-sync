//! Банковские сообщения из Messages (macOS) -> операции в Семейной бухгалтерии (family-budget).
//!
//! Команды:
//!   setup             интерактивная настройка
//!   set-token         сохранить API-токен в Keychain
//!   init              проверить связь, создать категории автоматических операций
//!   show [N]          показать последние N сообщений от банка и как они разобраны
//!   run [--dry-run] [--from-rowid N]   обработать новые сообщения
//!
//! Конфиг и состояние: ~/.config/family-messages-sync/{config,state}.json
//! (имя каталога и сервиса Keychain берётся из имени пакета в Cargo.toml)

mod api;
mod config;
mod dedupe;
mod keychain;
mod messages;
mod parse;
mod setup;
mod sync;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use std::{
    fs,
    io::{IsTerminal, stdin},
};

use config::{NAME, app_dir, config_path, load_config};

#[derive(Parser)]
#[command(version, about)] // имя и описание берутся из Cargo.toml
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Интерактивная настройка (создаёт или обновляет конфиг)
    Setup,
    /// Удалить конфиг, состояние и токен из Keychain и настроить заново
    Reset {
        /// не спрашивать подтверждение
        #[arg(long, short)]
        yes: bool,
    },
    /// Сохранить API-токен бухгалтерии в Keychain
    SetToken,
    /// Проверить токен, создать категории автоматических операций, проверить счета
    Init,
    /// Показать последние N сообщений от банка и как они разбираются
    Show {
        #[arg(default_value_t = 10)]
        n: usize,
    },
    /// Обработать новые сообщения и создать операции
    Run {
        /// только показать, что было бы сделано (бухгалтерия читается, но не меняется)
        #[arg(long)]
        dry_run: bool,
        /// начать с конкретного ROWID
        #[arg(long)]
        from_rowid: Option<i64>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup => return setup::cmd_setup(),
        Cmd::Reset { yes } => return setup::cmd_reset(yes),
        _ => {}
    }
    let (cfg, setup_ran) = load_config(setup::cmd_setup)?;
    // настройка уже сохранила токен и проверила связь
    if setup_ran && matches!(cli.cmd, Cmd::SetToken | Cmd::Init) {
        return Ok(());
    }
    let needs_account = !matches!(cli.cmd, Cmd::Show { .. } | Cmd::SetToken);
    if needs_account && cfg.account_codes().is_empty() {
        bail!("Не задан счёт по умолчанию в {}. Запустите: {NAME} setup", config_path().display());
    }
    if cfg.api_url.trim().is_empty() && needs_account && !stdin().is_terminal() {
        bail!("Не задан api_url в {}", config_path().display());
    }
    let matcher = parse::Matcher::new(&cfg)?;

    fs::create_dir_all(app_dir())?;
    // не запускаем два экземпляра одновременно
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(app_dir().join(".lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }

    match cli.cmd {
        Cmd::Setup | Cmd::Reset { .. } => unreachable!(),
        Cmd::SetToken => keychain::set(&cfg.keychain_service),
        Cmd::Init => setup::cmd_init(&cfg),
        Cmd::Show { n } => sync::cmd_show(&cfg, &matcher, n),
        Cmd::Run { dry_run, from_rowid } => sync::cmd_run(&cfg, &matcher, dry_run, from_rowid),
    }
}
