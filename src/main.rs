use anyhow::Result;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Crow: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    crow::cli::main_cli().await
}
