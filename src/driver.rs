//! Turns LLVM IR into a native executable by invoking clang, with a cache
//! (like Python's __pycache__) so re-running an unchanged script is instant.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const RUNTIME_C: &str = include_str!("../runtime/runtime.c");

pub struct BuildOptions {
    pub debug: bool,
    /// Developer mode: AddressSanitizer + UBSan, and every object a plain
    /// malloc, so reference-counting bugs show up as use-after-free reports.
    pub sanitize: bool,
}

impl BuildOptions {
    fn key(&self) -> [u8; 2] {
        [self.debug as u8, self.sanitize as u8]
    }

    fn opt_level(&self) -> &'static str {
        if self.debug { "-O0" } else if self.sanitize { "-O1" } else { "-O3" }
    }

    fn sanitizer_flags(&self) -> &'static [&'static str] {
        if self.sanitize { &["-fsanitize=address,undefined", "-fno-omit-frame-pointer", "-g", "-DISMA_SANITIZE"] } else { &[] }
    }
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
    Err("could not find `clang`, which Isma uses to produce native code.\n\
         Install it (Debian/Ubuntu: `sudo apt install clang`, Fedora: `sudo dnf install clang`,\n\
         macOS: `xcode-select --install`) or point ISMA_CC at a clang binary."
        .into())
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
    let dir = std::env::temp_dir().join(unique_name("isma"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{prefix}-{}-{nanos}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------- cache

/// `$ISMA_CACHE_DIR`, else `$XDG_CACHE_HOME/isma`, else `~/.cache/isma`.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ISMA_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(d).join("isma"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("isma"))
}

/// FNV-1a: tiny, and stable across Rust versions (unlike `DefaultHasher`).
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    pub fn write(&mut self, bytes: &[u8]) -> &mut Self {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        // separator so ("ab","c") and ("a","bc") hash differently
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        self
    }

    pub fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifies this exact compiler build, so a rebuilt compiler never
/// reuses binaries produced by an older one.
pub fn compiler_fingerprint() -> String {
    let mut h = Fnv::new();
    h.write(env!("CARGO_PKG_VERSION").as_bytes()).write(RUNTIME_C.as_bytes());
    if let Ok(meta) = std::env::current_exe().and_then(std::fs::metadata) {
        h.write(&meta.len().to_le_bytes());
        if let Ok(m) = meta.modified() {
            let t = m.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            h.write(&t.to_le_bytes());
        }
    }
    h.hex()
}

/// Write via a temp file + rename so concurrent runs never see half a file.
fn atomic_build(dest: &Path, build: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = dir.join(unique_name(".tmp"));
    let result = build(&tmp).and_then(|()| std::fs::rename(&tmp, dest).map_err(|e| format!("cannot write {}: {e}", dest.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The runtime compiled once to an object file and reused by every build.
fn cached_runtime_object(clang: &Path, opts: &BuildOptions) -> Option<PathBuf> {
    let mut h = Fnv::new();
    h.write(RUNTIME_C.as_bytes()).write(clang.as_os_str().as_encoded_bytes()).write(&opts.key());
    let obj = cache_dir()?.join(format!("runtime-{}.o", h.hex()));
    if obj.is_file() {
        return Some(obj);
    }
    let built = atomic_build(&obj, |tmp| {
        let src = tmp.with_extension("c");
        std::fs::write(&src, RUNTIME_C).map_err(|e| e.to_string())?;
        let out = Command::new(clang)
            .arg(opts.opt_level())
            .args(opts.sanitizer_flags())
            .args(["-w", "-c", "-x", "c"])
            .arg(&src)
            .arg("-o")
            .arg(tmp)
            .output();
        let _ = std::fs::remove_file(&src);
        match out {
            Ok(o) if o.status.success() => Ok(()),
            _ => Err("runtime compile failed".into()),
        }
    });
    built.ok().map(|()| obj)
}

/// Compile LLVM IR + the runtime into `output`.
pub fn build_executable(clang: &Path, ir: &str, output: &Path, opts: &BuildOptions) -> Result<(), String> {
    let dir = scratch_dir().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let result = (|| {
        let ll = dir.join("program.ll");
        std::fs::write(&ll, ir).map_err(|e| format!("cannot write {}: {e}", ll.display()))?;
        let runtime = match cached_runtime_object(clang, opts) {
            Some(obj) => obj,
            None => {
                let rt = dir.join("runtime.c");
                std::fs::write(&rt, RUNTIME_C).map_err(|e| format!("cannot write {}: {e}", rt.display()))?;
                rt
            }
        };
        let mut cmd = Command::new(clang);
        cmd.arg(opts.opt_level()).args(opts.sanitizer_flags());
        if opts.debug {
            cmd.arg("-g");
        }
        if opts.sanitize && runtime.extension().is_some_and(|e| e == "c") {
            cmd.arg("-DISMA_SANITIZE");
        }
        cmd.args(["-Wno-override-module", "-w", "-o"]).arg(output).arg(&ll).arg(&runtime).arg("-lm");
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

/// Where the cached executable for this script lives, plus the prefix shared
/// by all cached versions of the same script (used to prune stale ones).
/// `sources` identifies the contents (and shown paths) of the script and of
/// every file it imports, so editing any of them triggers a rebuild.
pub fn cached_program_path(script: &Path, sources: &str, opts: &BuildOptions) -> Option<(PathBuf, String)> {
    let abs = std::fs::canonicalize(script).unwrap_or_else(|_| script.to_path_buf());
    let mut script_key = Fnv::new();
    script_key.write(abs.as_os_str().as_encoded_bytes()).write(&opts.key());
    let mut content_key = Fnv::new();
    content_key.write(compiler_fingerprint().as_bytes()).write(sources.as_bytes()).write(&opts.key());
    let prefix = format!("{}-", script_key.hex());
    let path = cache_dir()?.join("bin").join(format!("{prefix}{}", content_key.hex()));
    Some((path, prefix))
}

/// Build `ir` into the cache slot `path`, removing older builds of the same script.
pub fn build_cached(clang: &Path, ir: &str, path: &Path, prefix: &str, opts: &BuildOptions) -> Result<(), String> {
    atomic_build(path, |tmp| build_executable(clang, ir, tmp, opts))?;
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
        && let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let n = e.file_name();
                let n = n.to_string_lossy();
                if n.starts_with(prefix) && *n != *name.to_string_lossy() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    Ok(())
}
