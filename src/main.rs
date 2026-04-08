use lambda_runtime::{run_concurrent, service_fn, Error};
use tracing_subscriber::fmt;
use tracing_subscriber::EnvFilter;

mod event_handler;
use event_handler::function_handler;


#[tokio::main]
async fn main() -> Result<(), Error> {
    // Structured JSON logging for CloudWatch Logs Insights querying.
    // Level controlled via RUST_LOG env var (default: info).
    fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_current_span(false)
        .without_time() // Lambda adds timestamps automatically
        .init();

    let func = service_fn(function_handler);
    run_concurrent(func).await?;
    
    Ok(())
}
