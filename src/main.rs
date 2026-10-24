//! Банковские сообщения из Messages (macOS) -> операции в Семейной бухгалтерии (family-budget).
//!
//! Команды:
//!   setup             интерактивная настройка
//!   set-token         сохранить API-токен в Keychain
//!   init              проверить связь, создать категории автоматических операций
//!   show [N]          показать последние N сообщений от банка и как они разобраны
//!   run [--dry-run] [--from-rowid N]   обработать новые сообщения
//!   log [N] [--status S]               журнал: что стало с последними сообщениями
//!   status            последний запуск, ошибка, очередь повторов, итоги за 7 дней
//!
//! Конфиг, состояние и журнал: ~/.config/family-messages-sync/{config.json,state.json,app.db}
//! Логи (unparsed.log, failed.log, out.log и err.log launchd) не растут больше 2 МБ: см. logs.rs
//! (имя каталога и сервиса Keychain берётся из имени пакета в Cargo.toml)

mod api;
mod config;
mod dedupe;
mod journal;
mod keychain;
mod logs;
mod messages;
mod notify;
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

use config::{Config, NAME, app_dir, config_path, load_config};

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
        /// начать с конкретного ROWID (старые сообщения при этом не отсекаются)
        #[arg(long)]
        from_rowid: Option<i64>,
    },
    /// Журнал: что стало с последними N сообщениями от банка
    Log {
        #[arg(default_value_t = 20)]
        n: usize,
        /// только с этим итогом
        #[arg(long, value_enum)]
        status: Option<journal::Status>,
    },
    /// Состояние: последний запуск, последняя ошибка, очередь повторов, итоги за 7 дней
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup => return setup::cmd_setup(),
        Cmd::Reset { yes } => return setup::cmd_reset(yes),
        Cmd::Status => return sync::cmd_status(),
        _ => {}
    }
    fs::create_dir_all(app_dir())?;
    // не запускаем два экземпляра одновременно; show и log только читают и не ждут
    let _lock = match cli.cmd {
        Cmd::Show { .. } | Cmd::Log { .. } => None,
        _ => match lock()? {
            Some(f) => Some(f),
            None => return Ok(()),
        },
    };
    // до конфига: если он не читается, err.log пополняется каждый запуск
    if matches!(cli.cmd, Cmd::Run { .. }) {
        logs::rotate_launchd();
    }
    let cfg = match prepare(&cli.cmd) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return Ok(()),
        Err(e) => {
            // фоновый запуск не должен молча вставать из-за конфига: уведомление и запись для `status`
            if matches!(cli.cmd, Cmd::Run { dry_run: false, .. }) {
                sync::report_failure(&e);
            }
            return Err(e);
        }
    };
    match cli.cmd {
        Cmd::Setup | Cmd::Reset { .. } | Cmd::Status => unreachable!(),
        Cmd::SetToken => keychain::set(&cfg.keychain_service),
        Cmd::Init => setup::cmd_init(&cfg),
        Cmd::Show { n } => sync::cmd_show(&cfg, &parse::Matcher::new(&cfg)?, n),
        Cmd::Log { n, status } => sync::cmd_log(&cfg, n, status),
        Cmd::Run { dry_run, from_rowid } => sync::cmd_run(&cfg, dry_run, from_rowid),
    }
}

/// Блокировка от второго экземпляра; `None` — уже работает другой.
fn lock() -> Result<Option<fs::File>> {
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(app_dir().join(".lock"))?;
    if f.try_lock_exclusive().is_err() {
        if stdin().is_terminal() {
            eprintln!("{NAME} уже работает в другом процессе (автозапуск?), повторите через минуту.");
        }
        return Ok(None);
    }
    Ok(Some(f))
}

/// Конфиг для команды; `None` — его только что создала первая настройка, и команда уже выполнена.
fn prepare(cmd: &Cmd) -> Result<Option<Config>> {
    let (cfg, setup_ran) = load_config(setup::cmd_setup)?;
    // настройка уже сохранила токен и проверила связь
    if setup_ran && matches!(cmd, Cmd::SetToken | Cmd::Init) {
        return Ok(None);
    }
    let needs_account = !matches!(cmd, Cmd::Show { .. } | Cmd::Log { .. } | Cmd::SetToken);
    if needs_account && cfg.account_codes().is_empty() {
        bail!("Не задан счёт по умолчанию в {}. Запустите: {NAME} setup", config_path().display());
    }
    if cfg.api_url.trim().is_empty() && needs_account && !stdin().is_terminal() {
        bail!("Не задан api_url в {}", config_path().display());
    }
    Ok(Some(cfg))
}
