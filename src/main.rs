use tarn::diagnostics::{self, Diagnostic, RenderFile};
use tarn::project::Project;
use tarn::driver::{self, BuildOptions};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
tarn  Python's syntax, Kotlin's strictness, native speed

Usage
  tarn <file.tr> [args...]          compile (cached) and run, like `python file.py`
  tarn run <file.tr> [args...]      the same, accepting --debug and --arc-stats
  tarn build <file.tr> [-o <out>]   build a standalone native binary
  tarn check <file.tr>              type-check and lint only
  tarn lsp                          the language server, for editors
  tarn clean                        empty the compile cache
  tarn doc --json                   the standard library and built-ins, as JSON (for tools)

Options
  -o <output>     output path (default: the file name without .tr)
  --debug         build with -O0 -g instead of -O3
  --arc-stats     print memory counters when the program exits
  --sanitize      build with AddressSanitizer
  --emit-llvm     also write the generated LLVM IR (.ll)
  --emit-mir      also write the mid-level IR, with reference counting (.mir)
  -h, --help      show this message
  -V, --version   show the compiler version

A script can start with `#!/usr/bin/env tarn` and run directly after `chmod +x`.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_cli(&args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("{} {msg}", red("error:"));
            ExitCode::from(1)
        }
    }
}

/// Colors on a terminal, unless `NO_COLOR` is set. `CLICOLOR_FORCE=1` asks for
/// them even when stderr isn't a terminal (tools that show the output themselves).
fn color_enabled() -> bool {
    let forced = std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0");
    std::env::var_os("NO_COLOR").is_none() && (forced || std::io::stderr().is_terminal())
}

fn red(s: &str) -> String {
    if color_enabled() { format!("\x1b[1;31m{s}\x1b[0m") } else { s.to_string() }
}

struct Cli {
    command: String,
    file: Option<PathBuf>,
    output: Option<PathBuf>,
    debug: bool,
    emit_llvm: bool,
    emit_mir: bool,
    arc_stats: bool,
    sanitize: bool,
    /// Arguments passed through to the program (`run` only).
    program_args: Vec<String>,
}

const COMMANDS: &[&str] = &["run", "build", "check", "clean", "lsp", "doc", "help", "version", "-h", "--help", "-V", "--version"];

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli =
        Cli { command: String::new(), file: None, output: None, debug: false, emit_llvm: false, emit_mir: false, arc_stats: false, sanitize: false, program_args: Vec::new() };
    let mut rest = args;
    match args.first() {
        // `tarn script.tr ...` is shorthand for `tarn run script.tr ...`
        Some(first) if !COMMANDS.contains(&first.as_str()) && !first.starts_with('-') => cli.command = "run".into(),
        Some(first) => {
            cli.command = first.clone();
            rest = &args[1..];
        }
        None => {}
    }
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if cli.command == "run" && cli.file.is_some() {
            // Like Python: everything after the script belongs to the script.
            // (A `--` right after the script is accepted and dropped.)
            if !(a == "--" && cli.program_args.is_empty()) {
                cli.program_args.push(a.clone());
            }
            cli.program_args.extend(it.by_ref().cloned());
            break;
        }
        match a.as_str() {
            "-o" => cli.output = Some(PathBuf::from(it.next().ok_or("`-o` needs a path")?)),
            "--debug" => cli.debug = true,
            "--emit-llvm" => cli.emit_llvm = true,
            "--emit-mir" => cli.emit_mir = true,
            "--arc-stats" => cli.arc_stats = true,
            "--sanitize" => cli.sanitize = true,
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`\n\n{USAGE}")),
            s if cli.file.is_none() => cli.file = Some(PathBuf::from(s)),
            s => return Err(format!("unexpected argument `{s}`")),
        }
    }
    Ok(cli)
}

fn run_cli(args: &[String]) -> Result<ExitCode, String> {
    if args.first().is_some_and(|a| a == "doc") {
        if args.len() != 2 || args[1] != "--json" {
            return Err("usage: tarn doc --json".into());
        }
        let json = serde_json::to_string_pretty(&tarn::doc::reference()).map_err(|e| e.to_string())?;
        println!("{json}");
        return Ok(ExitCode::SUCCESS);
    }
    let cli = parse_args(args)?;
    match cli.command.as_str() {
        "" | "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        "-V" | "--version" | "version" => {
            println!("tarn {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        "lsp" => {
            tarn::lsp::run().map_err(|e| format!("language server failed: {e}"))?;
            return Ok(ExitCode::SUCCESS);
        }
        "clean" => {
            if let Some(dir) = driver::cache_dir() {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove `{}`: {e}", dir.display()))?;
                }
                println!("removed {}", dir.display());
            }
            return Ok(ExitCode::SUCCESS);
        }
        "run" | "build" | "check" => {}
        other => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
    let file = cli.file.clone().ok_or_else(|| format!("missing input file\n\n{USAGE}"))?;
    let src = std::fs::read_to_string(&file).map_err(|e| format!("cannot read `{}`: {e}", file.display()))?;
    let display_path = file.display().to_string();
    let opts = BuildOptions { debug: cli.debug, sanitize: cli.sanitize };
    if cli.arc_stats {
        // Read by the runtime when the program exits.
        // SAFETY: still single-threaded; set before any program is spawned.
        unsafe { std::env::set_var("TARN_ARC_STATS", "1") };
    }

    // The script and every file it imports (parsing them is fast).
    let project = tarn::load_file(&file, &display_path, &src);

    if cli.command == "check" {
        return match tarn::check_project(&project) {
            Ok(checked) => {
                report_warnings(&checked.warnings, &project);
                Ok(ExitCode::SUCCESS)
            }
            Err(d) => Ok(report(&d, &project)),
        };
    }

    // Fast path: an unchanged script (imports included) that was already compiled runs immediately.
    let cached = if cli.command == "run" { driver::cached_program_path(&file, &project.fingerprint(), &opts) } else { None };
    if let Some((exe, _)) = &cached
        && exe.is_file() {
            return run_program(exe, &file, &cli.program_args);
        }

    let clang = driver::find_clang()?;
    let triple = driver::target_triple(&clang);
    let (ir, mir) = match tarn::compile_project(&project, triple.as_deref()) {
        Ok(c) => {
            report_warnings(&c.warnings, &project);
            (c.ir, c.mir)
        }
        Err(d) => return Ok(report(&d, &project)),
    };

    if cli.command == "build" {
        let output = cli.output.clone().unwrap_or_else(|| default_output(&file));
        if cli.emit_llvm {
            let ll = output.with_extension("ll");
            std::fs::write(&ll, &ir).map_err(|e| format!("cannot write `{}`: {e}", ll.display()))?;
        }
        if cli.emit_mir {
            let path = output.with_extension("mir");
            std::fs::write(&path, &mir).map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
        }
        driver::build_executable(&clang, &ir, &output, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }

    // run, with a cache slot
    if let Some((exe, prefix)) = &cached
        && driver::build_cached(&clang, &ir, exe, prefix, &opts).is_ok() {
            return run_program(exe, &file, &cli.program_args);
        }

    // run, no usable cache: build into a temp dir
    let dir = driver::scratch_dir().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let exe = dir.join("program");
    let result = driver::build_executable(&clang, &ir, &exe, &opts).and_then(|()| {
        Command::new(&exe).args(&cli.program_args).status().map_err(|e| format!("failed to run program: {e}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    Ok(exit_code_of(result?))
}

/// Run a compiled program. On Unix the tarn process is replaced by the
/// program (like `exec`), so signals, stdin and exit codes behave exactly
/// as if you had run the binary yourself.
fn run_program(exe: &Path, script: &Path, args: &[String]) -> Result<ExitCode, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.arg0(script).exec();
        Err(format!("failed to run program: {err}"))
    }
    #[cfg(not(unix))]
    {
        let _ = script;
        let status = cmd.status().map_err(|e| format!("failed to run program: {e}"))?;
        Ok(exit_code_of(status))
    }
}

fn default_output(file: &Path) -> PathBuf {
    let stem = file.file_stem().map(|s| s.to_os_string()).unwrap_or_else(|| "a.out".into());
    file.with_file_name(stem)
}

fn exit_code_of(status: std::process::ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(code as u8);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return ExitCode::from((128 + sig) as u8);
        }
    }
    ExitCode::from(1)
}

fn yellow(s: &str) -> String {
    if color_enabled() { format!("\x1b[1;33m{s}\x1b[0m") } else { s.to_string() }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}

/// Render diagnostics in source order (the entry file first, then the files it imports).
fn render_sorted(diags: &[Diagnostic], project: &Project) {
    let mut sorted = diags.to_vec();
    sorted.sort_by_key(|d| (d.span.start, !d.is_error()));
    let files: Vec<RenderFile> = project.files.iter().map(|f| RenderFile { name: &f.display, base: f.base, text: &f.text }).collect();
    eprint!("{}", diagnostics::render_files(&sorted, &files, color_enabled()));
}

fn report_warnings(warnings: &[Diagnostic], project: &Project) {
    if warnings.is_empty() {
        return;
    }
    render_sorted(warnings, project);
    let path = &project.files[0].display;
    eprintln!("{} `{path}` generated {}", yellow("warning:"), plural(warnings.len(), "warning"));
}

fn report(diags: &[Diagnostic], project: &Project) -> ExitCode {
    render_sorted(diags, project);
    let path = &project.files[0].display;
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.len() - errors;
    let extra = if warnings > 0 { format!("; {} emitted", plural(warnings, "warning")) } else { String::new() };
    eprintln!("{} could not compile `{path}` due to {}{extra}", red("error:"), plural(errors, "previous error"));
    ExitCode::from(1)
}
