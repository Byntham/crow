use anyhow::Result;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  Cause: {cause}");
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    crow::cli::main_cli().await
}
