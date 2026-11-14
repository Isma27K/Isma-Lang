//! Turns LLVM IR into a native executable by invoking clang.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const RUNTIME_C: &str = include_str!("../runtime/runtime.c");

pub struct BuildOptions {
    pub debug: bool,
}

/// Locate a clang binary: $ISMA_CC, then `clang` on PATH, then /usr/lib/llvm-*/bin/clang.
pub fn find_clang() -> Result<PathBuf, String> {
    if let Ok(cc) = std::env::var("ISMA_CC") {
        return Ok(PathBuf::from(cc));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join("clang");
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib")
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    versions.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("llvm-")));
    versions.sort();
    for v in versions.iter().rev() {
        let p = v.join("bin/clang");
        if p.is_file() {
            return Ok(p);
        }
    }
    Err("could not find `clang`; install it or set ISMA_CC=/path/to/clang".into())
}

/// The target triple clang compiles for (so the IR matches the host).
pub fn target_triple(clang: &Path) -> Option<String> {
    let out = Command::new(clang).arg("-dumpmachine").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// A unique scratch directory under the system temp dir.
pub fn scratch_dir() -> std::io::Result<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "isma-{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Compile LLVM IR + the runtime into `output`.
pub fn build_executable(clang: &Path, ir: &str, output: &Path, opts: &BuildOptions) -> Result<(), String> {
    let dir = scratch_dir().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let result = (|| {
        let ll = dir.join("program.ll");
        let rt = dir.join("runtime.c");
        std::fs::write(&ll, ir).map_err(|e| format!("cannot write {}: {e}", ll.display()))?;
        std::fs::write(&rt, RUNTIME_C).map_err(|e| format!("cannot write {}: {e}", rt.display()))?;
        let mut cmd = Command::new(clang);
        cmd.arg(if opts.debug { "-O0" } else { "-O3" });
        if opts.debug {
            cmd.arg("-g");
        }
        cmd.args(["-Wno-override-module", "-w", "-o"]).arg(output).arg(&ll).arg(&rt).arg("-lm");
        let out = cmd.output().map_err(|e| format!("failed to run {}: {e}", clang.display()))?;
        if !out.status.success() {
            return Err(format!(
                "clang failed (this is a bug in the Isma compiler):\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
