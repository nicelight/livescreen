use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Keeps the asynchronous file logger alive until application shutdown.
pub struct LoggingGuard {
    _file_guard: Option<WorkerGuard>,
}

/// Configures the process-wide logger.
///
/// Human-readable logs are written to stderr and to a daily rotating file.
/// Failure to create the file sink is deliberately non-fatal: stderr remains
/// available for diagnosing read-only or unusual runtime environments.
pub fn init() -> Result<LoggingGuard> {
    let filter = log_filter();
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_thread_names(true)
        .with_thread_ids(true)
        .with_target(true)
        .compact();

    let (file_writer, file_guard) = match create_file_writer() {
        Ok((writer, guard)) => (Some(writer), Some(guard)),
        Err(error) => {
            eprintln!("Не удалось включить логирование в файл: {error:#}");
            (None, None)
        }
    };

    if let Some(writer) = file_writer {
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_thread_names(true)
                    .with_thread_ids(true)
                    .with_target(true)
                    .with_writer(writer)
                    .compact(),
            )
            .try_init()
            .context("не удалось инициализировать систему логирования")?;
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .try_init()
            .context("не удалось инициализировать систему логирования")?;
    }

    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

fn log_filter() -> EnvFilter {
    let value = env::var("LIVESCREEN_LOG")
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".into());
    match EnvFilter::try_new(&value) {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("Некорректный фильтр логирования {value:?}: {error}; используется info");
            EnvFilter::new("info")
        }
    }
}

fn log_directory() -> PathBuf {
    if let Some(path) = env::var_os("LIVESCREEN_LOG_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("livescreen")
        .join("logs")
}

fn create_file_writer() -> Result<(NonBlocking, WorkerGuard)> {
    let directory = log_directory();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("не удалось создать каталог логов {}", directory.display()))?;
    let appender = tracing_appender::rolling::daily(&directory, "livescreen.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Ok((writer, guard))
}
