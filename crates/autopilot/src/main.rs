use clap::Parser;

#[tokio::main]
async fn main() {
    let args = autopilot::arguments::Arguments::parse();
    shared::tracing::initialize(args.log_filter.as_str(), args.log_stderr_threshold);
    tracing::info!("running autopilot with validated arguments:\n{}", args);
    shared::metrics::setup_metrics_registry(Some("gp_v2_autopilot".into()), None);
    autopilot::main(args).await;
}
