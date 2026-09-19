//! Run Crow's production reviewer on a local pinned comparison, without GitHub.
//! Usage: cargo run --example local_review -- SOURCE_JSON STATE_DIR IMAGE_ID_OR_AUTO PODMAN
//! Uses the operator's existing Crow provider configuration. Saves a report locally.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use tokio_util::sync::CancellationToken;

async fn until_shutdown<T>(
    review: impl std::future::Future<Output = T>,
    cancel: CancellationToken,
) -> Result<T> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::pin!(review);
    tokio::select! {
        result = &mut review => return Ok(result),
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
    }
    cancel.cancel();
    // Keep polling so provider and MCP process groups finish their cleanup.
    Ok(review.await)
}

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
    let cancel = CancellationToken::new();
    let result = until_shutdown(
        crow::provider::run_review(
            &job,
            &source,
            &guidance,
            root,
            crow::provider::Callbacks::default(),
            cancel.clone(),
        ),
        cancel,
    )
    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn signals_wait_for_detached_process_cleanup() {
        const CHILD: &str = "CROW_LOCAL_REVIEW_SIGNAL_CHILD";
        if let Some(dir) = std::env::var_os(CHILD) {
            let cancel = CancellationToken::new();
            let result = until_shutdown(
                crow::process::run(
                    "sh",
                    &["-c".into(), "echo $$ > child.pid; exec sleep 30".into()],
                    crow::process::RunOptions {
                        cwd: Some(dir.into()),
                        cancel: cancel.clone(),
                        ..Default::default()
                    },
                ),
                cancel,
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("Interrupted"));
            return;
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::signals_wait_for_detached_process_cleanup",
                ])
                .env(CHILD, dir.path())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(dir.path().join("child.pid"))
                        && let Ok(pid) = text.trim().parse::<i32>()
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(unsafe { libc::kill(child.id().unwrap() as i32, signal) }, 0);
            let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .expect("Runner did not finish process cleanup")
                .unwrap();
            assert!(status.success(), "Signal {signal}: {status}");
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
}
