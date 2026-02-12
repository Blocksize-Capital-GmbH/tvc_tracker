use tracing_subscriber::EnvFilter;

pub fn init_logging(log_dir: &str) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    // daily rotating file: logs/tvc_tracker.YYYY-MM-DD
    let file_appender = tracing_appender::rolling::daily(log_dir, "tvc_tracker.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false) // log files don't need rainbow control codes
        .init();

    Ok(guard) // keep this alive or logs may not flush
}
