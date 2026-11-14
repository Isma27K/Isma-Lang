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
///
/// Its name must stay outside the `isma-test-<name>-<pid>` paths `scratch`
/// hands out: it used to be `isma-test-cache-<pid>`, which is `scratch("cache")`,
/// so the cache test wiped it while other tests were linking against the
/// runtime object inside. Caches left behind by earlier runs are swept here.
fn cache_dir() -> PathBuf {
    const PREFIX: &str = "isma-shared-cache-";
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = std::env::temp_dir();
        let me = std::process::id();
        for e in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let pid = e.file_name().to_str().and_then(|n| n.strip_prefix(PREFIX)?.parse::<u32>().ok());
            if pid.is_some_and(|pid| pid != me && !process_alive(pid)) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        tmp.join(format!("{PREFIX}{me}"))
    })
    .clone()
}

fn process_alive(pid: u32) -> bool {
    // Only Linux can tell cheaply; elsewhere, keep every cache rather than
    // risk deleting one a concurrent test run is using.
    !cfg!(target_os = "linux") || Path::new("/proc").join(pid.to_string()).exists()
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

#[test]
fn cache_rebuilds_when_an_imported_file_changes() {
    let dir = scratch("cache-imports");
    let cache = dir.join("cache");
    let (main, helper) = (dir.join("main.isma"), dir.join("helper.isma"));
    let run = || Command::new(ISMA).arg(&main).env("ISMA_CACHE_DIR", &cache).output().unwrap();
    std::fs::write(&main, "from helper import value\nfn main():\n    print(value())\n").unwrap();
    std::fs::write(&helper, "fn value() -> Int:\n    return 1\n").unwrap();
    assert_eq!(String::from_utf8_lossy(&run().stdout), "1\n");
    std::fs::write(&helper, "fn value() -> Int:\n    return 2\n").unwrap();
    assert_eq!(String::from_utf8_lossy(&run().stdout), "2\n");
    // An error in the imported file is shown in that file.
    std::fs::write(&helper, "fn value() -> Int:\n    return \"3\"\n").unwrap();
    let out = run();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr.contains("helper.isma:2:12") && stderr.contains("mismatched types"), "{stderr}");
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

/// Editor support for enums: variant completion after `Shape.`, hovers on
/// variants in patterns, inlay hints for pattern bindings, and the outline.
#[test]
fn language_server_understands_enums() {
    let mut lsp = Lsp::start();
    let uri = "file:///tmp/enums.isma";
    let text = "enum Shape:\n    Circle(radius: Float)\n    Empty\n\nfn area(s: Shape) -> Float:\n    match s:\n        case Circle(r):\n            return r\n        case Empty:\n            return 0.0\n\nfn main():\n    print(area(Shape.))\n";
    lsp.open(uri, text);
    // `Shape.|`
    let items = lsp.at("textDocument/completion", uri, 12, 21);
    let circle = items["result"].as_array().unwrap().iter().find(|i| i["label"] == "Circle").unwrap_or_else(|| panic!("{items}"));
    assert_eq!(circle["insertText"], "Circle($1)");
    assert_eq!(circle["detail"], "Shape.Circle(radius: Float)");
    // hover on `Circle` in `case Circle(r):`
    let hover = lsp.at("textDocument/hover", uri, 6, 15);
    let shown = hover["result"]["contents"]["value"].as_str().unwrap();
    assert!(shown.contains("Shape.Circle(radius: Float)") && shown.contains("variant of `Shape`"), "{shown}");
    // `r` gets a type hint
    let hints = lsp.request("textDocument/inlayHint", serde_json::json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 13, "character": 0}}}));
    assert!(hints["result"].as_array().unwrap().iter().any(|h| h["label"] == ": Float" && h["position"]["line"] == 6), "{hints}");
    // outline: the enum with its variants
    let outline = lsp.request("textDocument/documentSymbol", serde_json::json!({"textDocument": {"uri": uri}}));
    let shape = &outline["result"][0];
    assert_eq!(shape["name"], "Shape");
    assert_eq!(shape["kind"], 10); // ENUM
    assert_eq!(shape["children"][0]["name"], "Circle");
    lsp.stop();
}

#[test]
fn language_server_works_across_files() {
    let dir = scratch("lsp-modules");
    let geometry = dir.join("geometry.isma");
    let geometry_text = "# Shapes and sizes.\n\nstruct Point:\n    x: Float\n    y: Float\n\n# The distance from the origin.\nfn norm(p: Point) -> Float:\n    return sqrt(p.x * p.x + p.y * p.y)\n\nfn _helper() -> Int:\n    return 1\n";
    std::fs::write(&geometry, geometry_text).unwrap();
    let main_text = "import geometry as geo\nfrom geometry import Point, norm\n\nfn main():\n    val p = Point(3.0, 4.0)\n    print(norm(p))\n    print(geo.norm(p))\n";
    let main = dir.join("main.isma");
    std::fs::write(&main, main_text).unwrap();
    let uri = |p: &Path| format!("file://{}", p.display());
    let (main_uri, geometry_uri) = (uri(&main), uri(&geometry));
    let mut lsp = Lsp::start();
    let diags = lsp.open(&main_uri, main_text);
    let messages: Vec<&str> = diags["params"]["diagnostics"].as_array().unwrap().iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert!(messages.iter().all(|m| !m.contains("module")), "{messages:?}");
    // Hover on an imported function shows where it comes from.
    let hover = lsp.at("textDocument/hover", &main_uri, 5, 11);
    let shown = hover["result"]["contents"]["value"].as_str().unwrap();
    assert!(shown.contains("fn norm(p: geometry.Point) -> Float") && shown.contains("from module `geometry`"), "{shown}");
    // Go to definition jumps into the other file.
    let def = lsp.at("textDocument/definition", &main_uri, 6, 15);
    assert_eq!(def["result"]["uri"], geometry_uri.as_str(), "{def}");
    assert_eq!(def["result"]["range"]["start"]["line"], 7);
    let def = lsp.at("textDocument/definition", &main_uri, 0, 10);
    assert_eq!(def["result"]["uri"], geometry_uri.as_str(), "{def}");
    // Editing the module (unsaved) re-checks the file that imports it.
    lsp.open(&geometry_uri, &geometry_text.replace("return sqrt(p.x * p.x + p.y * p.y)", "return \"far\""));
    let republished = loop {
        let msg = lsp.recv();
        if msg["method"] == "textDocument/publishDiagnostics" && msg["params"]["uri"] == main_uri.as_str() {
            break msg;
        }
    };
    let errors = republished["params"]["diagnostics"].as_array().unwrap();
    let import_error = errors.iter().find(|d| d["message"].as_str().unwrap().contains("an imported file has an error")).unwrap_or_else(|| panic!("{republished}"));
    assert_eq!(import_error["range"]["start"]["line"], 0);
    assert!(import_error["message"].as_str().unwrap().contains("geometry.isma:9:12: mismatched types"), "{import_error}");
    lsp.stop();

    // Completion inside `import` lines, in a fresh session (the files on disk).
    let mut lsp = Lsp::start();
    let text = "import \nfrom geometry import \nimport geometry as geo\nfn main():\n    geo.\n";
    lsp.open(&main_uri, text);
    // `geo.` lists the module's public names, through the alias.
    let items = lsp.at("textDocument/completion", &main_uri, 4, 8);
    let labels: Vec<&str> = items["result"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"norm") && labels.contains(&"Point") && !labels.contains(&"_helper"), "{labels:?}");
    let items = lsp.at("textDocument/completion", &main_uri, 0, 7);
    let labels: Vec<&str> = items["result"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"geometry") && labels.contains(&"time") && !labels.contains(&"main"), "{labels:?}");
    let items = lsp.at("textDocument/completion", &main_uri, 1, 21);
    let labels: Vec<&str> = items["result"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"Point") && labels.contains(&"norm") && !labels.contains(&"_helper"), "{labels:?}");
    lsp.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
