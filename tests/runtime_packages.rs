//! The host never extracts a cache. These tests run the sandbox helper against
//! synthetic temporary workspaces and inspect only its emitted archive bytes.
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

const HELPER: &str = include_str!("../src/runtime/packages.py");
const NPM: &str = ".crow-home/.npm/_cacache/content-v2/sha512";
const CARGO: &str = ".crow-home/.cargo/registry/cache/index.crates.io-1949cf8c6b5b557f";
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

fn assert_normalized_go_zip(root: &Path, relative: &str, pins: &Value, contents: Value) -> Vec<u8> {
    let path = root.join(relative);
    let inspected = Command::new("python3")
        .args(["-I", "-c", concat!(
            "import json,sys,zipfile\n",
            "with zipfile.ZipFile(sys.argv[1]) as z:\n",
            " assert z.testzip() is None\n",
            " assert not z.comment\n",
            " assert all(e.compress_type==zipfile.ZIP_DEFLATED and not e.extra and not e.comment and not(e.flag_bits & 8) and e.date_time==(1980,1,1,0,0,0) for e in z.infolist())\n",
            " print(json.dumps({e.filename:z.read(e).decode() for e in z.infolist()}))\n",
        )])
        .arg(&path).output().unwrap();
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&inspected.stdout).unwrap(),
        contents
    );
    let bytes = std::fs::read(path).unwrap();
    let exported = run("export", root, pins, &[]);
    assert!(exported.status.success());
    let repeated = tempfile::tempdir().unwrap();
    let imported = run("import", repeated.path(), pins, &exported.stdout);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        std::fs::read(repeated.path().join(relative)).unwrap(),
        bytes
    );
    bytes
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
    let pins = json!({"npm":true,"cargo":{"example-1.0.0.crate":{"source":"registry+https://github.com/rust-lang/crates.io-index","checksum":hex::encode(Sha256::digest(b"crate download"))}},"go":{"example.org/module/@v/v1.0.0.mod":h1("go.mod", module)}});
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
    let normalized = assert_normalized_go_zip(
        destination.path(),
        &relative,
        &correct_pins,
        json!({
            "example.org/module@v1.0.0/source.go":"package module\n",
            "example.org/module@v1.0.0/empty/":""
        }),
    );
    // An empty directory still needs a valid local ZIP header. Go validates it
    // before returning the empty stream; hashing only central metadata misses it.
    let corrupted = Command::new("python3")
        .args(["-I", "-c", concat!(
            "import pathlib,sys,zipfile\n",
            "path=pathlib.Path(sys.argv[1])\n",
            "with zipfile.ZipFile(path) as z: offset=next(entry.header_offset for entry in z.infolist() if entry.is_dir())\n",
            "data=bytearray(path.read_bytes()); data[offset:offset+4]=b'BAD!'; path.write_bytes(data)\n",
        )])
        .arg(&fullpath)
        .output()
        .unwrap();
    assert!(corrupted.status.success());
    let exported = run("export", source.path(), &correct_pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
    let corrupted_bytes = std::fs::read(&fullpath).unwrap();
    let imported = run(
        "import",
        destination.path(),
        &correct_pins,
        &archive(&relative, &corrupted_bytes, false),
    );
    assert!(!imported.status.success());
    assert_eq!(
        std::fs::read(destination.path().join(&relative)).unwrap(),
        normalized
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
fn go_zip_parser_filename_normalization_cannot_match_an_unrelated_pin() {
    let source = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let relative = format!("{GO}/{path}");
    let fullpath = source.path().join(&relative);
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    // All three archives previously hash a different filename in Python than
    // Go's raw ZIP name. Keep valid UTF-8 names as a positive control.
    for (kind, expected_name, accepted) in [
        ("nul", "source.go", false),
        ("cp437", "café.go", false),
        ("unicode-extra", "source.go", false),
        ("utf8", "café.go", true),
    ] {
        let fixture = Command::new("python3").args(["-I", "-c", concat!(
            "import pathlib,struct,sys,zipfile,zlib\n",
            "path=pathlib.Path(sys.argv[1]); kind=sys.argv[2]\n",
            "name='example.org/module@v1.0.0/'+{'nul':'source.goXsuffix','cp437':'cafX.go','unicode-extra':'different.go','utf8':'café.go'}[kind]\n",
            "info=zipfile.ZipInfo(name)\n",
            "if kind=='unicode-extra':\n",
            " extra=struct.pack('<BL',1,zlib.crc32(name.encode()))+b'example.org/module@v1.0.0/source.go'\n",
            " info.extra=struct.pack('<HH',0x7075,len(extra))+extra\n",
            "with zipfile.ZipFile(path,'w') as z: z.writestr(info,b'package module\\n')\n",
            "data=path.read_bytes()\n",
            "if kind=='nul': data=data.replace(b'source.goXsuffix',b'source.go\\0suffix')\n",
            "if kind=='cp437': data=data.replace(b'cafX.go',b'caf\\x82.go')\n",
            "path.write_bytes(data)\n",
        )]).arg(&fullpath).arg(kind).output().unwrap();
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
        let pin = h1(
            &format!("example.org/module@v1.0.0/{expected_name}"),
            b"package module\n",
        );
        // Actual HashZip results from the managed Go toolchain for these bytes.
        let go_checksum = match kind {
            "nul" => "h1:53GCxKW8aE9GoadhkD/+TMgCI1ew4Qac1EFAurIK7ZA=",
            "cp437" => "h1:i95QzR5fr/jaO44Iuh2b7rJOqDfgLfQlbzSQ6YMBoys=",
            "unicode-extra" => "h1:KzAbHc7OOrHukSjkRMRoFfSuCf//KMlLS4UwdKs83CM=",
            "utf8" => "h1:afHd4iduSgXHkM9J80TcYnbSVeRHHWH01YFfYqgNA2I=",
            _ => unreachable!(),
        };
        assert_eq!(pin == go_checksum, accepted, "{kind}");
        if kind == "nul" || kind == "unicode-extra" {
            // Same known answer independently checked with Go dirhash.HashZip
            // for the plain source.go fixture in the directory-entry regression.
            assert_eq!(pin, "h1://SsL+qsG2XNW4UV2fUFcFBpYyh+eYSho1uh0pTCTGM=");
        }
        let pins = json!({"go":{path:pin}});
        let exported = run("export", source.path(), &pins, &[]);
        assert!(
            exported.status.success(),
            "{kind}: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        assert_eq!(!names(&exported.stdout).is_empty(), accepted, "{kind}");
        let destination = tempfile::tempdir().unwrap();
        let bytes = std::fs::read(&fullpath).unwrap();
        let imported = run(
            "import",
            destination.path(),
            &pins,
            &archive(&relative, &bytes, false),
        );
        assert_eq!(
            imported.status.success(),
            accepted,
            "{kind}: {}",
            String::from_utf8_lossy(&imported.stderr)
        );
        assert_eq!(
            destination.path().join(&relative).exists(),
            accepted,
            "{kind}"
        );
        if accepted {
            assert_normalized_go_zip(
                destination.path(),
                &relative,
                &pins,
                json!({
                    format!("example.org/module@v1.0.0/{expected_name}"):"package module\n"
                }),
            );
        }
    }
}

#[test]
fn go_zip_cache_accepts_only_supported_compression_and_unencrypted_entries() {
    let source = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let relative = format!("{GO}/{path}");
    let fullpath = source.path().join(&relative);
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    // Independently verified with Go HashZip: Store and Deflate share this
    // checksum; BZIP2 and LZMA return "unsupported compression algorithm".
    let pins = json!({"go":{path:"h1://SsL+qsG2XNW4UV2fUFcFBpYyh+eYSho1uh0pTCTGM="}});
    for (method, flags, accepted) in [
        ("stored", 0, true),
        ("deflated", 0, true),
        ("bzip2", 0, false),
        ("lzma", 0, false),
        ("stored", 0x1, false),
        ("stored", 0x20, false),
        ("stored", 0x40, false),
        ("stored", 0x2000, false),
    ] {
        let fixture = Command::new("python3").args(["-I", "-c", concat!(
            "import pathlib,struct,sys,zipfile\n",
            "path=pathlib.Path(sys.argv[1]); method=sys.argv[2]; flags=int(sys.argv[3])\n",
            "compression={'stored':zipfile.ZIP_STORED,'deflated':zipfile.ZIP_DEFLATED,'bzip2':zipfile.ZIP_BZIP2,'lzma':zipfile.ZIP_LZMA}[method]\n",
            "with zipfile.ZipFile(path,'w',compression=compression) as z: z.writestr('example.org/module@v1.0.0/source.go',b'package module\\n')\n",
            "data=bytearray(path.read_bytes())\n",
            "for signature,offset in [(b'PK\\x03\\x04',6),(b'PK\\x01\\x02',8)]:\n",
            " position=data.index(signature)+offset\n",
            " struct.pack_into('<H',data,position,struct.unpack_from('<H',data,position)[0]|flags)\n",
            "path.write_bytes(data)\n",
        )]).arg(&fullpath).arg(method).arg(flags.to_string()).output().unwrap();
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
        let exported = run("export", source.path(), &pins, &[]);
        // One unsupported archive is skipped rather than aborting cache export.
        assert!(
            exported.status.success(),
            "{method}/{flags}: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        assert_eq!(
            !names(&exported.stdout).is_empty(),
            accepted,
            "{method}/{flags}"
        );
        let destination = tempfile::tempdir().unwrap();
        let bytes = std::fs::read(&fullpath).unwrap();
        let imported = run(
            "import",
            destination.path(),
            &pins,
            &archive(&relative, &bytes, false),
        );
        assert_eq!(
            imported.status.success(),
            accepted,
            "{method}/{flags}: {}",
            String::from_utf8_lossy(&imported.stderr)
        );
        assert_eq!(
            destination.path().join(&relative).exists(),
            accepted,
            "{method}/{flags}"
        );
        if accepted {
            assert_normalized_go_zip(
                destination.path(),
                &relative,
                &pins,
                json!({
                    "example.org/module@v1.0.0/source.go":"package module\n"
                }),
            );
        }
    }
}

#[test]
fn go_zip_metadata_is_rebuilt_before_export_and_import() {
    let source = tempfile::tempdir().unwrap();
    let path = "example.org/module/@v/v1.0.0.zip";
    let relative = format!("{GO}/{path}");
    let fullpath = source.path().join(&relative);
    std::fs::create_dir_all(fullpath.parent().unwrap()).unwrap();
    let pins = json!({"go":{path:"h1://SsL+qsG2XNW4UV2fUFcFBpYyh+eYSho1uh0pTCTGM="}});
    // Real Go rejects these original archives with "checksum error" and
    // "not a valid zip file", respectively. Their verified contents are intact.
    for kind in ["descriptor-crc", "understated-size"] {
        let fixture = Command::new("python3").args(["-I", "-c", concat!(
            "import io,pathlib,struct,sys,zipfile,zlib\n",
            "class Nonseekable(io.BytesIO):\n",
            " def seekable(self): return False\n",
            " def seek(self,*args): raise OSError('no seek')\n",
            "kind=sys.argv[2]; content=b'package module\\n'\n",
            "buffer=Nonseekable() if kind=='descriptor-crc' else io.BytesIO()\n",
            "with zipfile.ZipFile(buffer,'w') as z: z.writestr('example.org/module@v1.0.0/source.go',content+(b'hidden' if kind=='understated-size' else b''))\n",
            "data=bytearray(buffer.getvalue())\n",
            "if kind=='descriptor-crc': data[data.index(b'PK\\x07\\x08')+4]^=1\n",
            "else:\n",
            " local=data.index(b'PK\\x03\\x04'); central=data.index(b'PK\\x01\\x02')\n",
            " for offset in (local+14,central+16): struct.pack_into('<I',data,offset,zlib.crc32(content))\n",
            " for offset in (local+22,central+24): struct.pack_into('<I',data,offset,len(content))\n",
            "pathlib.Path(sys.argv[1]).write_bytes(data)\n",
        )]).arg(&fullpath).arg(kind).output().unwrap();
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
        let original = std::fs::read(&fullpath).unwrap();
        let exported = run("export", source.path(), &pins, &[]);
        assert!(
            exported.status.success(),
            "{}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let mut normalized = Vec::new();
        let mut archive_reader = tar::Archive::new(exported.stdout.as_slice());
        archive_reader
            .entries()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .read_to_end(&mut normalized)
            .unwrap();
        assert_ne!(normalized, original, "{kind}");
        let destination = tempfile::tempdir().unwrap();
        // Exercise import directly with original metadata too, including old cache entries.
        let raw_archive = archive(&relative, &original, false);
        let imported = run_budget(
            "import",
            destination.path(),
            &pins,
            &raw_archive,
            Some(normalized.len() as u64),
        );
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&imported.stdout).unwrap()["bytes"],
            normalized.len()
        );
        assert_eq!(
            std::fs::read(destination.path().join(&relative)).unwrap(),
            normalized
        );
        assert_normalized_go_zip(
            destination.path(),
            &relative,
            &pins,
            json!({
                "example.org/module@v1.0.0/source.go":"package module\n"
            }),
        );
        let too_small = tempfile::tempdir().unwrap();
        let imported = run_budget(
            "import",
            too_small.path(),
            &pins,
            &raw_archive,
            Some(normalized.len() as u64 - 1),
        );
        assert!(imported.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&imported.stdout).unwrap(),
            json!({"files":0,"bytes":0,"skipped":1})
        );
        assert!(!too_small.path().join(&relative).exists());
    }
}

#[test]
fn normalized_go_zip_output_limit_is_enforced_during_writing() {
    // A stored random payload expands slightly under Deflate. Lowering the test
    // limit exercises the same bounded writer without constructing a 128 MiB ZIP.
    let output = Command::new("python3").args(["-I", "-c", concat!(
        "import base64,hashlib,io,random,sys,zipfile\n",
        "namespace={'__name__':'cache_test'}; exec(sys.argv[1],namespace)\n",
        "name='example.org/module@v1.0.0/random.bin'; content=random.Random(7).randbytes(8192)\n",
        "buffer=io.BytesIO()\n",
        "with zipfile.ZipFile(buffer,'w') as z: z.writestr(name,content)\n",
        "data=buffer.getvalue()\n",
        "line=(hashlib.sha256(content).hexdigest()+'  '+name+'\\n').encode()\n",
        "pin='h1:'+base64.b64encode(hashlib.sha256(line).digest()).decode()\n",
        "normalized=namespace['checked_download'](data,('gozip',pin))\n",
        "assert len(normalized)>len(data)\n",
        "namespace['FILE_LIMIT']=len(data)\n",
        "try: namespace['checked_download'](data,('gozip',pin))\n",
        "except ValueError as error: assert 'exceeds limit' in str(error),error\n",
        "else: raise AssertionError('Normalized ZIP exceeded output limit')\n",
    )]).arg(HELPER).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cargo_and_go_pins_reject_changed_download_contents() {
    let root = tempfile::tempdir().unwrap();
    let cargo = format!("{CARGO}/example-1.0.0.crate");
    let gomod = format!("{GO}/example.org/module/@v/v1.0.0.mod");
    let pins = json!({"cargo":{"example-1.0.0.crate":{"source":"registry+https://github.com/rust-lang/crates.io-index","checksum":hex::encode(Sha256::digest(b"approved"))}},"go":{"example.org/module/@v/v1.0.0.mod":h1("go.mod", b"approved")}});
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

#[test]
fn cargo_cache_rejects_legacy_ambiguous_checksum_lists() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = format!("{CARGO}/same-1.0.0.crate");
    let original = b"registry A package";
    let substituted = b"registry B package";
    write(source.path(), &path, substituted);
    let pins = json!({"cargo":{"same-1.0.0.crate":[hex::encode(Sha256::digest(original)),hex::encode(Sha256::digest(substituted))]}});
    let exported = run("export", source.path(), &pins, &[]);
    assert!(exported.status.success());
    assert!(names(&exported.stdout).is_empty());
    let imported = run(
        "import",
        destination.path(),
        &pins,
        &archive(&path, substituted, false),
    );
    assert!(!imported.status.success());
    assert!(!destination.path().join(&path).exists());
    let precise = json!({"cargo":{"same-1.0.0.crate":{"source":"registry+https://github.com/rust-lang/crates.io-index","checksum":hex::encode(Sha256::digest(original))}}});
    let imported = run(
        "import",
        destination.path(),
        &precise,
        &archive(&path, original, false),
    );
    assert!(imported.status.success());
    assert_eq!(
        std::fs::read(destination.path().join(&path)).unwrap(),
        original
    );
    let imported = run(
        "import",
        destination.path(),
        &precise,
        &archive(&path, substituted, false),
    );
    assert!(!imported.status.success());
    assert_eq!(
        std::fs::read(destination.path().join(&path)).unwrap(),
        original
    );
}

#[test]
fn cargo_archive_pins_cannot_cross_registry_directories_or_sources() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let content = b"archive from a pinned registry";
    let checksum = hex::encode(Sha256::digest(content));
    let canonical = "registry+https://github.com/rust-lang/crates.io-index";
    let alternate = "registry+sparse+https://generated.example/index/";
    // Setup can create alternate-registry lockfiles that are absent from pinned
    // Git. Even matching bytes cannot cross an unauthorized registry directory.
    for (directory, registry, accepted) in [
        ("index.crates.io-1949cf8c6b5b557f", canonical, true),
        ("generated.example-aabbccdd", canonical, false),
        ("index.crates.io-1949cf8c6b5b557f", alternate, false),
        ("generated.example-aabbccdd", alternate, false),
        ("github.com-1ecc6299db9ec823", canonical, false),
    ] {
        let relative = format!(".crow-home/.cargo/registry/cache/{directory}/example-1.0.0.crate");
        let pins = json!({"cargo":{"example-1.0.0.crate":{"source":registry,"checksum":checksum}}});
        write(source.path(), &relative, content);
        let exported = run("export", source.path(), &pins, &[]);
        assert!(exported.status.success());
        assert_eq!(
            names(&exported.stdout).contains(&relative),
            accepted,
            "{directory} {registry}"
        );
        let imported = run(
            "import",
            destination.path(),
            &pins,
            &archive(&relative, content, false),
        );
        assert_eq!(
            imported.status.success(),
            accepted,
            "{directory} {registry}"
        );
        if accepted {
            assert_eq!(
                std::fs::read(destination.path().join(&relative)).unwrap(),
                content
            );
        }
        std::fs::remove_file(source.path().join(&relative)).unwrap();
    }
    // Legacy filename-only pins fail closed even with a single valid checksum.
    let relative = format!("{CARGO}/example-1.0.0.crate");
    let legacy = json!({"cargo":{"example-1.0.0.crate":[checksum]}});
    assert!(
        !run(
            "import",
            destination.path(),
            &legacy,
            &archive(&relative, content, false)
        )
        .status
        .success()
    );
}
