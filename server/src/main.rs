use anyhow::Result;
use clap::Parser;

use ains_server::bootstrap::CliArgs;

#[tokio::main]
async fn main() -> Result<()> {
    let cli_args = CliArgs::parse();

    ains_server::bootstrap::init_logging(&cli_args.log_level);
    ains_server::bootstrap::setup_panic_handler();

    let bootstrap_result = ains_server::bootstrap::bootstrap(cli_args).await?;
    ains_server::bootstrap::start_server(bootstrap_result).await?;

    Ok(())
}
