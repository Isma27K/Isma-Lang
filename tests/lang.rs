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

fn isma(args: &[&str]) -> Output {
    Command::new(ISMA).args(args).env("NO_COLOR", "1").output().expect("failed to run isma")
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
