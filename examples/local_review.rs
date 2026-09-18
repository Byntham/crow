//! Run Crow's production reviewer on a local pinned comparison, without GitHub.
//! Usage: cargo run --example local_review -- SOURCE_JSON STATE_DIR IMAGE_ID_OR_AUTO PODMAN
//! Uses the operator's existing Crow provider configuration. Saves a report locally.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // prepare_review launches the current executable as its MCP helper.
    if args.get(1).is_some_and(|s| s == "_inspection-mcp") {
        return crow::inspection::inspection_main(
            Path::new(args.get(2).context("Missing source path")?),
            args.get(3).map(Path::new),
        )
        .await;
    }
    anyhow::ensure!(
        args.len() == 5,
        "Usage: local_review SOURCE_JSON STATE_DIR IMAGE_ID_OR_AUTO PODMAN"
    );
    let source: Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let root = Path::new(&args[2]);
    let mut settings = crow::config::load(&crow::config::home())?["worker"].clone();
    settings
        .as_object_mut()
        .context("Worker settings")?
        .remove("token");
    settings.as_object_mut().unwrap().remove("id");
    settings["subagents"] = json!({"mode":"inherit","max":0});
    settings["timeoutMs"] = json!(600000);
    let repo = source["repo"].as_str().unwrap_or("fixture/checkout");
    let job_id = source["jobId"].as_str().unwrap_or("visual-checkout-review");
    let number = source["number"].as_u64().unwrap_or(1);
    settings["execution"] = json!({"podman":args[4],"repositories":{repo:{"image":args[3],"timeoutSeconds":120,"memoryMiB":1536,"workspaceMiB":512,"cpus":2,"pids":256,"maxRuns":12}}});
    let job = json!({"id":job_id,"repo":repo,"number":number,"comparison":source,"settings":settings,"prContext":source.get("prContext").cloned().unwrap_or_else(||json!({"title":"Refresh the checkout button finish asset","body":"Refresh the checkout button artwork. The checkout flow and displayed price should remain unchanged."}))});
    let guidance = crow::inspection::guidance(&source).await?;
    let result = crow::provider::run_review(
        &job,
        &source,
        &guidance,
        root,
        crow::provider::Callbacks::default(),
        CancellationToken::new(),
    )
    .await;
    match result {
        Ok(report) => {
            crow::util::atomic(&root.join("result.json"), &report)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Err(error) => {
            std::fs::create_dir_all(root)?;
            std::fs::write(root.join("error.txt"), error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}
