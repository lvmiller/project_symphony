use tracing_subscriber::EnvFilter;

pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .without_time()
        .compact()
        .try_init();
}

pub fn redact_secret(value: &str) -> &'static str {
    let _ = value;
    "[REDACTED]"
}
