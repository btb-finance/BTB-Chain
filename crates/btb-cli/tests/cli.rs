//! End-to-end: `btb init` makes a project that builds, deploy bundles are correct, and `btb dev`
//! hot reloads while keeping state. Needs the riscv64 target (like the other end-to-end tests).

use btb_cli::{build, deploy, init, sha256_hex, DevSession, SdkSource};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn sdk() -> SdkSource {
    SdkSource::Path(concat!(env!("CARGO_MANIFEST_DIR"), "/../btb-sdk").into())
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("btb-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn http(addr: SocketAddr, method: &str, target: &str, body: &str) -> (String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_string(), body.to_string())
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(240) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn new_project(tag: &str) -> (PathBuf, PathBuf) {
    let dir = temp(tag);
    let root = init(&dir, "hello", &sdk()).unwrap();
    (dir, root)
}

#[test]
fn init_scaffold_matches_todo_structure_and_rejects_bad_names() {
    let (dir, root) = new_project("scaffold");
    for f in [
        "Cargo.toml",
        ".cargo/config.toml",
        "link.ld",
        "rust-toolchain.toml",
        "src/main.rs",
        "web/index.html",
    ] {
        assert!(root.join(f).is_file(), "missing {f}");
    }
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("name = \"hello\"") && cargo.contains("btb-sdk = { path = "));
    assert!(init(&dir, "hello", &sdk()).is_err(), "non-empty dir");
    assert!(init(&dir, "Bad Name", &sdk()).is_err());
    let git = init(
        &dir,
        "g",
        &SdkSource::Git("https://example.com/btb.git".into()),
    )
    .unwrap();
    assert!(std::fs::read_to_string(git.join("Cargo.toml"))
        .unwrap()
        .contains("git = \"https://example.com/btb.git\""));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn binary_init_then_build_lists_entry_points_and_deploy_hash_is_correct() {
    let dir = temp("bin");
    let btb = env!("CARGO_BIN_EXE_btb");
    let run = |args: &[&str], cwd: &Path| {
        let o = Command::new(btb)
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "btb {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let sdk_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../btb-sdk");
    run(&["init", "hello", "--sdk-path", sdk_dir], &dir);
    let root = dir.join("hello");
    let out = run(&["build"], &root);
    for e in ["mutation add", "mutation toggle", "query list_mine"] {
        assert!(out.contains(e), "{out}");
    }
    let elf = std::fs::read(root.join("target/btb/hello.elf")).unwrap();
    let out = run(&["deploy"], &root);
    assert!(
        out.contains("Phase 4") && out.contains(&sha256_hex(&elf)),
        "{out}"
    );
    let bundle = root.join("target/btb/deploy");
    assert_eq!(std::fs::read(bundle.join("hello.elf")).unwrap(), elf);
    let manifest = std::fs::read_to_string(bundle.join("manifest.json")).unwrap();
    assert!(manifest.contains(&format!("\"code_hash\": \"{}\"", sha256_hex(&elf))));
    assert!(manifest.contains("{\"kind\": \"query\", \"name\": \"list_mine\"}"));
    assert!(manifest.contains("Note:owner:addr@0"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sha256_known_vector() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn library_build_and_deploy() {
    let (dir, root) = new_project("lib");
    let b = build(&root).unwrap();
    assert!(b.entry_points.iter().any(|e| e.name == "add"));
    let (b2, bundle) = deploy(&root).unwrap();
    assert_eq!(b.elf, b2.elf, "build is reproducible");
    assert!(bundle.join("manifest.json").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dev_hot_reload_keeps_state_and_adds_function() {
    let (dir, root) = new_project("dev");
    let s = DevSession::start(&root, ("127.0.0.1", 0), Duration::from_millis(100), None).unwrap();
    let addr = s.addr();
    let (st, body) = http(
        addr,
        "POST",
        "/mutation/add?format=json&caller=alice",
        "[\"first\"]",
    );
    assert!(st.contains("200"), "{st} {body}");
    let list = |who: &str| {
        http(
            addr,
            "GET",
            &format!("/query/list_mine?format=json&caller={who}"),
            "",
        )
    };
    assert!(list("alice").1.contains("first"));
    // Not there yet.
    assert!(http(addr, "GET", "/query/count_mine?caller=alice", "")
        .0
        .contains("404"));

    // A broken edit is reported and the old build keeps running.
    let src = root.join("src/main.rs");
    let orig = std::fs::read_to_string(&src).unwrap();
    std::fs::write(&src, format!("{orig}\nthis is not rust\n")).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    wait_for("failed build to settle", || {
        list("alice").1.contains("first")
    });
    assert_eq!(s.reload_count(), 0);

    // Add a query.
    let extra = "\n#[btb::query]\npub fn count_mine() -> u64 {\n    list_mine().len() as u64\n}\n";
    std::fs::write(&src, format!("{orig}{extra}")).unwrap();
    wait_for("hot reload", || s.reload_count() == 1);
    let (st, body) = http(
        addr,
        "GET",
        "/query/count_mine?format=json&caller=alice",
        "",
    );
    assert!(st.contains("200"), "{st} {body}");
    assert!(body.contains("01"), "count is 1 (hex 0100..): {body}");
    assert!(list("alice").1.contains("first"), "old state retained");
    s.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dev_reload_reruns_subscriptions_and_pushes_changes() {
    use std::io::{BufRead, BufReader};
    let (dir, root) = new_project("sub");
    let s = DevSession::start(&root, ("127.0.0.1", 0), Duration::from_millis(100), None).unwrap();
    let addr = s.addr();
    http(addr, "POST", "/mutation/add?caller=bob", "[\"hello\"]");
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(240))).unwrap();
    write!(
        c,
        "GET /subscribe/list_mine?format=json&caller=bob HTTP/1.1\r\nHost: x\r\n\r\n"
    )
    .unwrap();
    let mut r = BufReader::new(c);
    let mut next_event = move || loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        if let Some(d) = l.strip_prefix("data: ") {
            return d.to_string();
        }
    };
    assert!(next_event().contains("hello"));
    let src = root.join("src/main.rs");
    let text = std::fs::read_to_string(&src).unwrap();
    assert!(text.contains("out.push((id, t.done, t.text));"));
    std::fs::write(
        &src,
        text.replace(
            "out.push((id, t.done, t.text));",
            "out.push((id, t.done, t.text.to_uppercase()));",
        ),
    )
    .unwrap();
    assert!(
        next_event().contains("HELLO"),
        "reload pushes the changed result"
    );
    s.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
