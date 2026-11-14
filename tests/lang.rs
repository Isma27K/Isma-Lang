//! End-to-end language tests.
//!
//! - tests/run/*.isma    compile + run; stdout must equal the sibling `.out` file
//! - tests/fail/*.isma   must fail to compile; first line `# expect-error: <text>`
//! - tests/panic/*.isma  must exit 101; first line `# expect-panic: <text>`
//! - tests/arc/*.isma    must stay within an allocation budget; first line `# expect-max-allocs: <n>`
//!
//! Every `run` program must also free everything it allocated (checked by the
//! runtime with `ISMA_ARC_CHECK=1`), and is run again under AddressSanitizer.

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

fn isma_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(ISMA);
    cmd.args(args).env("NO_COLOR", "1").env("ISMA_CACHE_DIR", cache_dir());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run isma")
}

/// Run a `tests/run` program and compare its output; also fail on leaks.
fn check_run(f: &Path, extra: &[&str]) -> Result<(), String> {
    let mut args = vec!["run"];
    args.extend_from_slice(extra);
    args.push(f.to_str().unwrap());
    let out = isma_env(&args, &[("ISMA_ARC_CHECK", "1")]);
    let expected =
        std::fs::read_to_string(f.with_extension("out")).map_err(|e| format!("{}: missing .out file ({e})", f.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("{}: exited with {}\n{stderr}", f.display(), out.status));
    }
    if stdout != expected {
        return Err(format!("{}: output mismatch\n--- expected\n{expected}\n--- got\n{stdout}", f.display()));
    }
    for bad in ["objects leaked", "runtime error", "AddressSanitizer", "LeakSanitizer"] {
        if stderr.contains(bad) {
            return Err(format!("{}: {bad}\n{stderr}", f.display()));
        }
    }
    Ok(())
}

#[test]
fn run_programs() {
    run_all(cases("run"), |f| check_run(f, &[]));
}

/// Can this clang build programs with AddressSanitizer?
fn sanitizer_available() -> bool {
    let Ok(clang) = isma::driver::find_clang() else { return false };
    let dir = scratch("asan-probe");
    let src = dir.join("probe.c");
    std::fs::write(&src, "int main(void) { return 0; }\n").unwrap();
    let ok = Command::new(clang)
        .args(["-fsanitize=address,undefined", "-o"])
        .arg(dir.join("probe"))
        .arg(&src)
        .output()
        .is_ok_and(|o| o.status.success());
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

/// Reference-counting bugs (use after free, double free) are caught by ASan:
/// in `--sanitize` builds every object is its own malloc.
#[test]
fn run_programs_under_sanitizers() {
    if !sanitizer_available() {
        eprintln!("skipping: clang can't build with -fsanitize=address,undefined");
        return;
    }
    run_all(cases("run"), |f| check_run(f, &["--sanitize"]));
}

#[test]
fn allocation_budgets() {
    run_all(cases("arc"), |f| {
        let max: u64 = directive(f, "expect-max-allocs").parse().map_err(|e| format!("{}: bad budget: {e}", f.display()))?;
        let out = isma_env(&["run", f.to_str().unwrap()], &[("ISMA_ARC_STATS", "1"), ("ISMA_ARC_CHECK", "1")]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() || stderr.contains("objects leaked") {
            return Err(format!("{}: failed\n{stderr}", f.display()));
        }
        let allocs: u64 = stderr
            .lines()
            .find_map(|l| l.strip_prefix("isma arc: "))
            .and_then(|l| l.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| format!("{}: no `isma arc:` stats line in\n{stderr}", f.display()))?;
        if allocs > max {
            return Err(format!("{}: {allocs} allocations, budget is {max}", f.display()));
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
    // Other test threads spawning processes can briefly inherit our write
    // handle to the script, making exec fail with ETXTBSY: retry that case.
    let out = (0..100)
        .find_map(|_| match Command::new(&script).arg("a").env("PATH", &path).env("ISMA_CACHE_DIR", cache_dir()).output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
            other => Some(other.unwrap()),
        })
        .expect("script stayed busy");
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

/// A language-server session over stdio, exactly like an editor's.
struct Lsp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Lsp {
    fn start() -> Lsp {
        use std::process::Stdio;
        let mut child = Command::new(ISMA).arg("lsp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut lsp = Lsp { child, stdin, stdout, next_id: 100 };
        let init = lsp.request("initialize", serde_json::json!({"capabilities": {}}));
        assert!(init["result"]["capabilities"]["hoverProvider"].as_bool().unwrap());
        lsp.send(serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        lsp
    }

    fn send(&mut self, msg: serde_json::Value) {
        use std::io::Write;
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> serde_json::Value {
        use std::io::{BufRead, Read};
        let mut len = 0;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length: ") {
                len = v.parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// Send a request and return its response (skipping notifications).
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let msg = self.recv();
            if msg["id"] == id {
                return msg;
            }
        }
    }

    /// Open a document and return its published diagnostics.
    fn open(&mut self, uri: &str, text: &str) -> serde_json::Value {
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": uri, "languageId": "isma", "version": 1, "text": text}}}));
        let diags = self.recv();
        assert_eq!(diags["method"], "textDocument/publishDiagnostics");
        diags
    }

    fn at(&mut self, method: &str, uri: &str, line: u32, character: u32) -> serde_json::Value {
        self.request(method, serde_json::json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}))
    }

    fn stop(mut self) {
        assert!(self.request("shutdown", serde_json::Value::Null)["error"].is_null());
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
        assert!(self.child.wait().unwrap().success());
    }
}

/// Talk to `isma lsp` over stdio exactly like an editor would.
#[test]
fn language_server_protocol_roundtrip() {
    let mut lsp = Lsp::start();
    let uri = "file:///tmp/demo.isma";
    let text = "fn main():\n    val n = 1 + 2.0\n    val unused = 3\n";
    let diags = lsp.open(uri, text);
    let list = diags["params"]["diagnostics"].as_array().unwrap();
    let messages: Vec<&str> = list.iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert!(messages.iter().any(|m| m.contains("cannot be applied to `Int` and `Float`")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("unused variable `unused`")), "{messages:?}");
    // the error is on line 1 (0-based), at `1 + 2.0`
    let err = list.iter().find(|d| d["severity"] == 1).unwrap();
    assert_eq!(err["range"]["start"], serde_json::json!({"line": 1, "character": 12}));

    let hover = lsp.at("textDocument/hover", uri, 2, 9);
    assert!(hover["result"]["contents"]["value"].as_str().unwrap().contains("val unused: Int"), "{hover}");
    lsp.stop();
}

/// Editor support for nullable types: `?.` completion, `.` completion that
/// inserts `?.`, and hovers that show smart casts.
#[test]
fn language_server_understands_nullable_types() {
    let mut lsp = Lsp::start();
    let uri = "file:///tmp/nulls.isma";
    let text = "struct User:\n    name: String\n\nfn find() -> User?:\n    return null\n\nfn main():\n    val u = find()\n    print(u?.)\n    print(u.)\n    if u != null:\n        print(u.name)\n";
    lsp.open(uri, text);
    let labels = |r: &serde_json::Value| -> Vec<String> {
        r["result"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap().to_string()).collect()
    };
    // `u?.|`
    let safe = lsp.at("textDocument/completion", uri, 8, 13);
    assert!(labels(&safe).contains(&"name".to_string()), "{safe}");
    assert!(safe["result"][0]["additionalTextEdits"].is_null(), "{safe}");
    // `u.|` offers the same members and rewrites the `.` into `?.`
    let plain = lsp.at("textDocument/completion", uri, 9, 12);
    assert!(labels(&plain).contains(&"name".to_string()), "{plain}");
    let edit = &plain["result"][0]["additionalTextEdits"][0];
    assert_eq!(edit["newText"], "?.", "{plain}");
    assert_eq!(edit["range"]["start"], serde_json::json!({"line": 9, "character": 11}));
    // hover: declared `User?`, smart-cast to `User` inside the `if`
    let decl = lsp.at("textDocument/hover", uri, 7, 8);
    assert!(decl["result"]["contents"]["value"].as_str().unwrap().contains("val u: User?"), "{decl}");
    let smart = lsp.at("textDocument/hover", uri, 11, 14);
    let shown = smart["result"]["contents"]["value"].as_str().unwrap();
    assert!(shown.contains("val u: User") && !shown.contains("User?`\n") && shown.contains("smart cast"), "{shown}");
    lsp.stop();
}
