//! The host never extracts a cache. These tests run the sandbox helper against
//! synthetic temporary workspaces and inspect only its emitted archive bytes.
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const HELPER: &str = include_str!("../src/runtime/packages.py");
const NPM: &str = ".crow-home/.npm/_cacache/content-v2/sha512";
const CARGO: &str = ".crow-home/.cargo/registry/cache/registry";
const GO: &str = ".crow-home/go/pkg/mod/cache/download";

fn run(mode: &str, root: &Path, pins: &Value, input: &[u8]) -> std::process::Output {
    run_budget(mode, root, pins, input, None)
}
fn run_budget(
    mode: &str,
    root: &Path,
    pins: &Value,
    input: &[u8],
    budget: Option<u64>,
) -> std::process::Output {
    let mut command = Command::new("python3");
    command
        .args(["-I", "-c", HELPER, mode, &pins.to_string()])
        .arg(root);
    if let Some(budget) = budget {
        command.arg(budget.to_string());
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is required for runtime helper verification");
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn npm_path(data: &[u8]) -> String {
    let digest = hex::encode(Sha512::digest(data));
    format!("{NPM}/{}/{}/{}", &digest[..2], &digest[2..4], &digest[4..])
}
fn write(root: &Path, path: &str, data: &[u8]) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}
fn archive(path: &str, data: &[u8], symlink: bool) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    if symlink {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("/tmp/escaped").unwrap();
        header.set_size(0);
    } else {
        header.set_size(data.len() as u64);
    }
    if path.starts_with("../") || path.starts_with('/') {
        // Raw header bytes deliberately allow traversal paths in rejection tests.
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder
            .append(&header, if symlink { &[][..] } else { data })
            .unwrap();
    } else {
        builder
            .append_data(&mut header, path, if symlink { &[][..] } else { data })
            .unwrap();
    }
    builder.into_inner().unwrap()
}
fn names(bytes: &[u8]) -> Vec<String> {
    tar::Archive::new(bytes)
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}
fn h1(name: &str, content: &[u8]) -> String {
    let line = format!("{}  {name}\n", hex::encode(Sha256::digest(content)));
    format!(
        "h1:{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(line.as_bytes()))
    )
}

#[test]
fn verified_downloads_roundtrip_without_metadata_installed_sources_or_outputs() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let npm = npm_path(b"registry package");
    let cargo = format!("{CARGO}/example-1.0.0.crate");
    let gomod = format!("{GO}/example.org/module/@v/v1.0.0.mod");
    let module = b"module example.org/module\n";
    let pins = json!({"npm":true,"cargo":{"example-1.0.0.crate":[hex::encode(Sha256::digest(b"crate download"))]},"go":{"example.org/module/@v/v1.0.0.mod":h1("go.mod", module)}});
    for (path, data) in [
        (&npm, b"registry package".as_slice()),
        (&cargo, b"crate download"),
        (&gomod, module),
    ] {
        write(source.path(), path, data);
    }
    for path in [
        "tracked.js",
        "node_modules/package/index.js",
        ".crow-home/.npm/_cacache/index-v5/metadata",
        ".crow-home/.cargo/registry/src/registry/package/lib.rs",
        ".crow-home/go/pkg/mod/cache/download/example.org/module/@v/v1.0.0.ziphash",
    ] {
        write(source.path(), path, b"must not transfer");
    }
    let exported = run("export", source.path(), &pins, &[]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(
        names(&exported.stdout),
        vec![npm.clone(), cargo.clone(), gomod.clone()]
    );
    let imported = run("import", destination.path(), &pins, &exported.stdout);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        std::fs::read(destination.path().join(npm)).unwrap(),
        b"registry package"
    );
    assert_eq!(
        std::fs::read(destination.path().join(cargo)).unwrap(),
        b"crate download"
    );
    assert_eq!(
        std::fs::read(destination.path().join(gomod)).unwrap(),
        module
    );
    assert!(!destination.path().join("tracked.js").exists());
}

#[test]
fn poisoned_downloads_are_not_exported_and_fail_import() {
    let root = tempfile::tempdir().unwrap();
    let path = npm_path(b"trusted download");
    write(root.path(), &path, b"poison");
    let pins = json!({"npm":true});
    let exported = run("export", root.path(), &pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
    let imported = run(
        "import",
        root.path(),
        &pins,
        &archive(&path, b"poison", false),
    );
    assert!(!imported.status.success());
    assert!(String::from_utf8_lossy(&imported.stderr).contains("integrity mismatch"));
}

#[test]
fn unsafe_paths_links_and_unpinned_packages_fail_import() {
    let root = tempfile::tempdir().unwrap();
    for (path, link) in [
        ("../outside", false),
        ("/tmp/outside", false),
        ("tracked.js", false),
        (
            ".crow-home/.cargo/registry/cache/registry/unpinned-1.crate",
            false,
        ),
        (&npm_path(b"data"), true),
    ] {
        let imported = run(
            "import",
            root.path(),
            &json!({"npm":true}),
            &archive(path, b"data", link),
        );
        assert!(!imported.status.success(), "Accepted {path}");
    }
}

#[cfg(unix)]
#[test]
fn symlink_ancestors_and_hardlinked_blobs_cannot_escape_workspace() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join(".crow-home")).unwrap();
    let path = npm_path(b"package");
    let pins = json!({"npm":true});
    let imported = run(
        "import",
        root.path(),
        &pins,
        &archive(&path, b"package", false),
    );
    assert!(!imported.status.success());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let exported = run("export", root.path(), &pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
    std::fs::remove_file(root.path().join(".crow-home")).unwrap();
    write(root.path(), &path, b"package");
    std::fs::hard_link(root.path().join(&path), outside.path().join("hardlink")).unwrap();
    let exported = run("export", root.path(), &pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
    let imported = run(
        "import",
        root.path(),
        &pins,
        &archive(&path, b"package", false),
    );
    assert!(imported.status.success());
    assert_eq!(
        std::fs::metadata(outside.path().join("hardlink"))
            .unwrap()
            .len(),
        7
    );
    assert_ne!(
        std::fs::metadata(outside.path().join("hardlink"))
            .unwrap()
            .ino(),
        std::fs::metadata(root.path().join(&path)).unwrap().ino()
    );
    std::fs::remove_file(root.path().join(&path)).unwrap();
    std::fs::write(outside.path().join("target"), b"do not overwrite").unwrap();
    symlink(outside.path().join("target"), root.path().join(&path)).unwrap();
    let imported = run(
        "import",
        root.path(),
        &pins,
        &archive(&path, b"package", false),
    );
    assert!(imported.status.success());
    assert_eq!(
        std::fs::read(outside.path().join("target")).unwrap(),
        b"do not overwrite"
    );
}

#[test]
fn go_zip_checksums_cover_entry_names_and_contents() {
    let root = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let fullpath = root.path().join(format!("{GO}/{path}"));
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    let filename = "example.org/module@v1.0.0/source.go";
    let output = Command::new("python3").args(["-I", "-c", "import sys,zipfile\nwith zipfile.ZipFile(sys.argv[1], 'w') as z: z.writestr(sys.argv[2], b'package module\\n')"])
        .arg(&fullpath).arg(filename).output().unwrap();
    assert!(output.status.success());
    let pins = json!({"go":{path:h1(filename, b"package module\n")}});
    let exported = run("export", root.path(), &pins, &[]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(names(&exported.stdout), vec![format!("{GO}/{path}")]);
    let poisoned_pins = json!({"go":{path:h1(filename, b"different source")}});
    let exported = run("export", root.path(), &poisoned_pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
}

#[test]
fn go_zip_directory_entries_match_go_dirhash_and_invalidate_old_pins() {
    // Independently computed with the managed image's real Go implementation:
    // golang.org/x/mod/sumdb/dirhash.HashZip(path, dirhash.Hash1).
    const PLAIN: &str = "h1://SsL+qsG2XNW4UV2fUFcFBpYyh+eYSho1uh0pTCTGM=";
    const WITH_DIRECTORY: &str = "h1:29Kh5CWmDNDUwzyafYH8DQ8rA9DyPcmG15u+tQtiSpA=";
    assert_eq!(
        h1("example.org/module@v1.0.0/source.go", b"package module\n"),
        PLAIN
    );
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let relative = format!("{GO}/{path}");
    let fullpath = source.path().join(&relative);
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    let fixture = Command::new("python3")
        .args([
            "-I",
            "-c",
            concat!(
                "import sys,zipfile\n",
                "with zipfile.ZipFile(sys.argv[1], 'w') as z:\n",
                " z.writestr('example.org/module@v1.0.0/source.go', b'package module\\n')\n",
                " z.writestr('example.org/module@v1.0.0/empty/', b'')\n",
            ),
        ])
        .arg(&fullpath)
        .output()
        .unwrap();
    assert!(fixture.status.success());
    let bytes = std::fs::read(&fullpath).unwrap();
    let stale_pins = json!({"go":{path:PLAIN}});
    let exported = run("export", source.path(), &stale_pins, &[]);
    assert!(exported.status.success());
    assert!(
        names(&exported.stdout).is_empty(),
        "Adding a directory must change the Go checksum"
    );
    let imported = run(
        "import",
        destination.path(),
        &stale_pins,
        &archive(&relative, &bytes, false),
    );
    assert!(!imported.status.success());
    assert!(String::from_utf8_lossy(&imported.stderr).contains("integrity mismatch"));
    assert!(!destination.path().join(&relative).exists());
    let correct_pins = json!({"go":{path:WITH_DIRECTORY}});
    let exported = run("export", source.path(), &correct_pins, &[]);
    assert!(exported.status.success());
    assert_eq!(names(&exported.stdout), vec![relative.clone()]);
    let imported = run(
        "import",
        destination.path(),
        &correct_pins,
        &exported.stdout,
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        std::fs::read(destination.path().join(&relative)).unwrap(),
        bytes
    );
}

#[test]
fn malformed_go_zip_directories_cannot_hide_from_checksum_validation() {
    let root = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let relative = format!("{GO}/{path}");
    let fullpath = root.path().join(&relative);
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    let pins = json!({"go":{path:h1("example.org/module@v1.0.0/source.go", b"package module\n")}});
    for (directory, content) in [
        ("example.org/module@v1.0.0/nonempty/", "data"),
        ("example.org/module@v1.0.0/new\nline/", ""),
    ] {
        let fixture = Command::new("python3")
            .args([
                "-I",
                "-c",
                concat!(
                    "import sys,zipfile\n",
                    "with zipfile.ZipFile(sys.argv[1], 'w') as z:\n",
                    " z.writestr('example.org/module@v1.0.0/source.go', b'package module\\n')\n",
                    " z.writestr(sys.argv[2], sys.argv[3].encode())\n",
                ),
            ])
            .arg(&fullpath)
            .arg(directory)
            .arg(content)
            .output()
            .unwrap();
        assert!(fixture.status.success());
        let exported = run("export", root.path(), &pins, &[]);
        assert!(exported.status.success());
        assert!(names(&exported.stdout).is_empty());
        let bytes = std::fs::read(&fullpath).unwrap();
        let destination = tempfile::tempdir().unwrap();
        let imported = run(
            "import",
            destination.path(),
            &pins,
            &archive(&relative, &bytes, false),
        );
        assert!(!imported.status.success());
        assert!(!destination.path().join(&relative).exists());
    }
}

#[test]
fn cargo_and_go_pins_reject_changed_download_contents() {
    let root = tempfile::tempdir().unwrap();
    let cargo = format!("{CARGO}/example-1.0.0.crate");
    let gomod = format!("{GO}/example.org/module/@v/v1.0.0.mod");
    let pins = json!({"cargo":{"example-1.0.0.crate":[hex::encode(Sha256::digest(b"approved"))]},"go":{"example.org/module/@v/v1.0.0.mod":h1("go.mod", b"approved")}});
    for path in [&cargo, &gomod] {
        write(root.path(), path, b"poison");
        let imported = run(
            "import",
            root.path(),
            &pins,
            &archive(path, b"poison", false),
        );
        assert!(!imported.status.success());
        assert!(String::from_utf8_lossy(&imported.stderr).contains("integrity mismatch"));
    }
    let exported = run("export", root.path(), &pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
}

#[test]
fn import_budget_keeps_room_for_source_and_installation() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let pins = json!({"npm":true});
    for data in [b"package one", b"package two"] {
        write(source.path(), &npm_path(data), data);
    }
    let exported = run("export", source.path(), &pins, &[]);
    assert!(exported.status.success());
    let entries = names(&exported.stdout);
    assert_eq!(entries.len(), 2);
    let imported = run_budget(
        "import",
        destination.path(),
        &pins,
        &exported.stdout,
        Some(11),
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&imported.stdout).unwrap(),
        json!({"files":1,"bytes":11,"skipped":1})
    );
    assert!(destination.path().join(&entries[0]).exists());
    assert!(!destination.path().join(&entries[1]).exists());
    let empty = tempfile::tempdir().unwrap();
    let imported = run_budget("import", empty.path(), &pins, &exported.stdout, Some(0));
    assert!(imported.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&imported.stdout).unwrap(),
        json!({"files":0,"bytes":0,"skipped":2})
    );
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
fn oversized_members_are_rejected_before_reading_their_contents() {
    let root = tempfile::tempdir().unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_path("oversized").unwrap();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(129 * 1024 * 1024);
    header.set_cksum();
    let mut input = header.as_bytes().to_vec();
    input.extend([0; 1024]);
    let imported = run("import", root.path(), &json!({"npm":true}), &input);
    assert!(!imported.status.success());
    assert!(
        String::from_utf8_lossy(&imported.stderr).contains("Invalid package cache archive entry")
    );
    let imported = run_budget(
        "import",
        root.path(),
        &json!({}),
        &[],
        Some(513 * 1024 * 1024),
    );
    assert!(!imported.status.success());
    assert!(
        String::from_utf8_lossy(&imported.stderr).contains("Invalid package cache import budget")
    );
}
