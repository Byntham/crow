//! Seccomp profile for experiment containers. Podman's default profile lets a
//! container create user namespaces, the usual first step of kernel privilege
//! escalation. Crow derives its profile from that default and removes namespace
//! creation: `unshare` and `setns` are dropped, `clone3` fails with ENOSYS so
//! libc falls back to `clone`, and `clone` is allowed only without namespace flags.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

const NAMESPACE_SYSCALLS: &[&str] = &["clone", "clone3", "unshare", "setns"];
/// CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET
const NAMESPACE_FLAGS: u64 = 0x7E02_0000;
const ENOSYS: u64 = 38;

/// Crow's profile, derived from the one Podman applies by default, which
/// `podman info` reports as `host.security.seccompProfilePath`.
pub(super) fn derive(host: &Value) -> Result<Value> {
    let path = host["security"]["seccompProfilePath"]
        .as_str()
        .unwrap_or("");
    ensure!(
        !path.is_empty(),
        "Podman reports no seccomp profile file to derive Crow's profile from"
    );
    let default = crate::util::read_json(Path::new(path))?
        .with_context(|| format!("Podman's seccomp profile {path} is missing"))?;
    harden(&default)
}

pub(super) fn harden(default: &Value) -> Result<Value> {
    // On these architectures the clone flags are the first argument.
    ensure!(
        cfg!(any(target_arch = "x86_64", target_arch = "aarch64")),
        "Runtime experiments support x86_64 and aarch64 hosts only"
    );
    let mut profile = default.clone();
    ensure!(
        profile["defaultAction"]
            .as_str()
            .is_some_and(|a| a != "SCMP_ACT_ALLOW"),
        "Podman's seccomp profile allows system calls by default"
    );
    let rules = profile["syscalls"]
        .as_array_mut()
        .context("Podman's seccomp profile has no system call rules")?;
    let namespace = |name: &Value| {
        name.as_str()
            .is_some_and(|n| NAMESPACE_SYSCALLS.contains(&n))
    };
    for rule in rules.iter_mut() {
        if let Some(names) = rule["names"].as_array_mut() {
            names.retain(|n| !namespace(n));
        }
    }
    // Rules may also use the older single `name` field.
    rules.retain(|rule| {
        !namespace(&rule["name"]) && !rule["names"].as_array().is_some_and(Vec::is_empty)
    });
    rules.push(json!({
        "names": ["clone"],
        "action": "SCMP_ACT_ALLOW",
        "args": [{"index": 0, "value": NAMESPACE_FLAGS, "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}],
    }));
    rules.push(json!({"names": ["clone3"], "action": "SCMP_ACT_ERRNO", "errnoRet": ENOSYS}));
    Ok(profile)
}

/// Shell check, run before any repository file is unpacked, that the container
/// cannot create a user namespace.
pub(super) const CHECK: &str = r#"python3 -I -c 'import os, sys
try:
    os.unshare(os.CLONE_NEWUSER)
except OSError:
    sys.exit(0)
sys.exit(1)' || { echo 'Crow could not block user namespaces in the container' >&2; exit 125; }"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_calls_are_removed_and_clone_is_restricted() {
        let default = json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "syscalls": [
                {"names": ["read", "clone", "clone3", "unshare"], "action": "SCMP_ACT_ALLOW"},
                {"names": ["setns"], "action": "SCMP_ACT_ALLOW", "includes": {"caps": ["CAP_SYS_ADMIN"]}},
                {"name": "unshare", "action": "SCMP_ACT_ALLOW"},
            ],
        });
        let profile = harden(&default).unwrap();
        let rules = profile["syscalls"].as_array().unwrap();
        assert_eq!(rules[0]["names"], json!(["read"]));
        assert_eq!(
            rules.len(),
            3,
            "emptied and single-name rules are dropped: {profile}"
        );
        assert_eq!(rules[1]["names"], json!(["clone"]));
        assert_eq!(rules[1]["args"][0]["value"], NAMESPACE_FLAGS);
        assert_eq!(rules[2]["names"], json!(["clone3"]));
        assert_eq!(rules[2]["errnoRet"], ENOSYS);
        let text = profile.to_string();
        assert!(!text.contains("\"unshare\"") && !text.contains("\"setns\""));
        assert!(harden(&json!({"defaultAction":"SCMP_ACT_ALLOW","syscalls":[]})).is_err());
        assert!(harden(&json!({"defaultAction":"SCMP_ACT_ERRNO"})).is_err());
    }
}
