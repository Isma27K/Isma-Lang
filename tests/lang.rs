//! End-to-end language tests.
//!
//! - tests/run/*.isma    compile + run; stdout must equal the sibling `.out` file
//! - tests/fail/*.isma   must fail to compile; first line `# expect-error: <text>`
//! - tests/panic/*.isma  must exit 101; first line `# expect-panic: <text>`

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ISMA: &str = env!("CARGO_BIN_EXE_isma");

fn cases(dir: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "isma"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no test cases in {}", root.display());
    files
}

/// A cache private to this test process, so every test run really compiles.
fn cache_dir() -> PathBuf {
    std::env::temp_dir().join(format!("isma-test-cache-{}", std::process::id()))
}

fn isma(args: &[&str]) -> Output {
    Command::new(ISMA).args(args).env("NO_COLOR", "1").env("ISMA_CACHE_DIR", cache_dir()).output().expect("failed to run isma")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("isma-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn directive(path: &Path, key: &str) -> String {
    let src = std::fs::read_to_string(path).unwrap();
    let first = src.lines().next().unwrap_or_default();
    first
        .strip_prefix(&format!("# {key}:"))
        .unwrap_or_else(|| panic!("{}: first line must be `# {key}: ...`", path.display()))
        .trim()
        .to_string()
}

/// Run `check` on every case in parallel and collect failure messages.
fn run_all(files: Vec<PathBuf>, check: fn(&Path) -> Result<(), String>) {
    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = files.iter().map(|f| s.spawn(move || check(f).err())).collect();
        handles.into_iter().filter_map(|h| h.join().unwrap()).collect()
    });
    if !failures.is_empty() {
        panic!("{} case(s) failed:\n\n{}", failures.len(), failures.join("\n\n"));
    }
}

#[test]
fn run_programs() {
    run_all(cases("run"), |f| {
        let out = isma(&["run", f.to_str().unwrap()]);
        let expected = std::fs::read_to_string(f.with_extension("out"))
            .map_err(|e| format!("{}: missing .out file ({e})", f.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Err(format!("{}: exited with {}\n{}", f.display(), out.status, String::from_utf8_lossy(&out.stderr)));
        }
        if stdout != expected {
            return Err(format!("{}: output mismatch\n--- expected\n{expected}\n--- got\n{stdout}", f.display()));
        }
        Ok(())
    });
}

#[test]
fn compile_errors() {
    run_all(cases("fail"), |f| {
        let want = directive(f, "expect-error");
        let out = isma(&["check", f.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            return Err(format!("{}: compiled successfully but should have failed with `{want}`", f.display()));
        }
        if !stderr.contains(&want) {
            return Err(format!("{}: expected error containing `{want}`, got:\n{stderr}", f.display()));
        }
        Ok(())
    });
}

#[test]
fn runtime_panics() {
    run_all(cases("panic"), |f| {
        let want = directive(f, "expect-panic");
        let out = isma(&["run", f.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(101) {
            return Err(format!("{}: expected exit code 101, got {}\n{stderr}", f.display(), out.status));
        }
        if !stderr.contains(&want) || !stderr.contains("panic at ") {
            return Err(format!("{}: expected panic containing `{want}`, got:\n{stderr}", f.display()));
        }
        Ok(())
    });
}

#[test]
fn build_produces_native_executable() {
    let dir = std::env::temp_dir().join(format!("isma-build-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("fib");
    let ll = dir.join("fib.ll");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fib.isma");
    let out = isma(&["build", src.to_str().unwrap(), "-o", exe.to_str().unwrap(), "--emit-llvm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let run = Command::new(&exe).output().unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("fib(30) = 832040"));
    let ir = std::fs::read_to_string(&ll).unwrap();
    assert!(ir.contains("define internal i64 @isma_fn_fib(i64 %a0)"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_style_invocation_with_args() {
    let dir = scratch("args");
    let script = dir.join("echo.isma");
    std::fs::write(
        &script,
        "fn main():\n    val a = args()\n    print(a.len())\n    for x in a:\n        print(x)\n",
    )
    .unwrap();
    // `isma file.isma ...`: everything after the script goes to the script,
    // including things that look like isma flags.
    let out = isma(&[script.to_str().unwrap(), "hello", "--debug", "two words"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3\nhello\n--debug\ntwo words\n");
    // `isma run file.isma -- x` still works.
    let out = isma(&["run", script.to_str().unwrap(), "--", "x"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\nx\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_reuses_and_invalidates() {
    let dir = scratch("cache");
    let cache = dir.join("cache");
    let script = dir.join("prog.isma");
    let run = || {
        Command::new(ISMA).arg(&script).env("ISMA_CACHE_DIR", &cache).output().unwrap()
    };
    std::fs::write(&script, "fn main():\n    print(1)\n").unwrap();
    assert_eq!(String::from_utf8_lossy(&run().stdout), "1\n");
    assert_eq!(String::from_utf8_lossy(&run().stdout), "1\n"); // cache hit
    // Editing the script must trigger a rebuild, and the old build is pruned.
    std::fs::write(&script, "fn main():\n    print(2)\n").unwrap();
    assert_eq!(String::from_utf8_lossy(&run().stdout), "2\n");
    let bins = std::fs::read_dir(cache.join("bin")).unwrap().count();
    assert_eq!(bins, 1, "stale cached binaries should be removed");
    // Compile errors are still reported (and never cached).
    std::fs::write(&script, "fn main():\n    val x: Int = \"no\"\n").unwrap();
    let out = run();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("mismatched types"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn shebang_scripts_run_directly() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("shebang");
    // Put the freshly built isma first on PATH so `#!/usr/bin/env isma` finds it.
    let bin = Path::new(ISMA).parent().unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let script = dir.join("hello.isma");
    std::fs::write(&script, "#!/usr/bin/env isma\nfn main():\n    print(f\"hi {args().len()}\")\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(&script).arg("a").env("PATH", path).env("ISMA_CACHE_DIR", cache_dir()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi 1\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lint_warnings() {
    run_all(cases("warn"), |f| {
        let want = directive(f, "expect-warning");
        let out = isma(&["check", f.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            return Err(format!("{}: warnings must not fail compilation:\n{stderr}", f.display()));
        }
        if !stderr.contains(&want) || !stderr.contains("Warning") {
            return Err(format!("{}: expected warning containing `{want}`, got:\n{stderr}", f.display()));
        }
        Ok(())
    });
}

#[test]
fn underscore_silences_lints_and_can_repeat() {
    let dir = scratch("underscore");
    let f = dir.join("u.isma");
    std::fs::write(&f, "fn main():\n    for _ in 0..2:\n        print(1)\n    val _ = 5\n    val _ = 6\n    val _unused = 7\n").unwrap();
    let out = isma(&["check", f.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "expected no warnings: {}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Talk to `isma lsp` over stdio exactly like an editor would.
#[test]
fn language_server_protocol_roundtrip() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;

    let mut child = Command::new(ISMA).arg("lsp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut send = |msg: serde_json::Value| {
        let body = msg.to_string();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    };
    let mut recv = || -> serde_json::Value {
        let mut len = 0;
        loop {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length: ") {
                len = v.parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    };

    let uri = "file:///tmp/demo.isma";
    let text = "fn main():\n    val n = 1 + 2.0\n    val unused = 3\n";
    send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}}));
    let init = recv();
    assert!(init["result"]["capabilities"]["hoverProvider"].as_bool().unwrap());
    send(serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    send(serde_json::json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": uri, "languageId": "isma", "version": 1, "text": text}}}));
    let diags = recv();
    assert_eq!(diags["method"], "textDocument/publishDiagnostics");
    let list = diags["params"]["diagnostics"].as_array().unwrap();
    let messages: Vec<&str> = list.iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert!(messages.iter().any(|m| m.contains("cannot be applied to `Int` and `Float`")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("unused variable `unused`")), "{messages:?}");
    // the error is on line 1 (0-based), at `1 + 2.0`
    let err = list.iter().find(|d| d["severity"] == 1).unwrap();
    assert_eq!(err["range"]["start"], serde_json::json!({"line": 1, "character": 12}));

    send(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {
        "textDocument": {"uri": uri}, "position": {"line": 2, "character": 9}}}));
    let hover = recv();
    assert!(hover["result"]["contents"]["value"].as_str().unwrap().contains("val unused: Int"), "{hover}");

    send(serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null}));
    assert_eq!(recv()["id"], 3);
    send(serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
    assert!(child.wait().unwrap().success());
}
