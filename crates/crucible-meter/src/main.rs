//! Standalone meter binary; same as `crucible meter`.

use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Metering proxy; reads {\"api_key\",\"endpoint\"} as one JSON line on stdin"
)]
struct Cli {
    #[command(flatten)]
    args: crucible_meter::MeterArgs,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match crucible_meter::run(Cli::parse().args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("meter: {e}");
            std::process::ExitCode::from(2)
        }
    }
}
