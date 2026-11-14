use isma::diagnostics::{self, Diagnostic};
use isma::driver::{self, BuildOptions};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
Isma — Python syntax, Kotlin-strict types, native speed.

USAGE:
    isma run   <file.isma> [--debug] [-- <program args>]
    isma build <file.isma> [-o <output>] [--debug] [--emit-llvm]
    isma check <file.isma>

OPTIONS:
    -o <output>    Output executable path (default: file name without .isma)
    --debug        Build with -O0 -g instead of -O3
    --emit-llvm    Also write the generated LLVM IR next to the output (.ll)
    -h, --help     Show this message
    -V, --version  Show the compiler version
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

fn color_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli { command: String::new(), file: None, output: None, debug: false, emit_llvm: false };
    let mut it = args.iter();
    cli.command = it.next().cloned().unwrap_or_default();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => cli.output = Some(PathBuf::from(it.next().ok_or("`-o` needs a path")?)),
            "--debug" => cli.debug = true,
            "--emit-llvm" => cli.emit_llvm = true,
            "--" => break,
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`\n\n{USAGE}")),
            s => {
                if cli.file.is_some() {
                    return Err(format!("unexpected argument `{s}`"));
                }
                cli.file = Some(PathBuf::from(s));
            }
        }
    }
    Ok(cli)
}

fn run_cli(args: &[String]) -> Result<ExitCode, String> {
    let cli = parse_args(args)?;
    match cli.command.as_str() {
        "" | "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        "-V" | "--version" | "version" => {
            println!("isma {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        "run" | "build" | "check" => {}
        other => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
    let file = cli.file.clone().ok_or_else(|| format!("missing input file\n\n{USAGE}"))?;
    let src = std::fs::read_to_string(&file).map_err(|e| format!("cannot read `{}`: {e}", file.display()))?;
    let display_path = file.display().to_string();

    if cli.command == "check" {
        return match isma::check_source(&src) {
            Ok(_) => Ok(ExitCode::SUCCESS),
            Err(d) => Ok(report(&d, &display_path, &src)),
        };
    }

    let clang = driver::find_clang()?;
    let triple = driver::target_triple(&clang);
    let ir = match isma::compile_to_ir(&src, &display_path, triple.as_deref()) {
        Ok(ir) => ir,
        Err(d) => return Ok(report(&d, &display_path, &src)),
    };
    let opts = BuildOptions { debug: cli.debug };

    if cli.command == "build" {
        let output = cli.output.clone().unwrap_or_else(|| default_output(&file));
        if cli.emit_llvm {
            let ll = output.with_extension("ll");
            std::fs::write(&ll, &ir).map_err(|e| format!("cannot write `{}`: {e}", ll.display()))?;
        }
        driver::build_executable(&clang, &ir, &output, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }

    // run
    let dir = driver::scratch_dir().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let exe = dir.join("program");
    let built = driver::build_executable(&clang, &ir, &exe, &opts);
    let result = built.and_then(|()| {
        let pos = args.iter().position(|a| a == "--");
        let prog_args: &[String] = pos.map(|p| &args[p + 1..]).unwrap_or(&[]);
        Command::new(&exe).args(prog_args).status().map_err(|e| format!("failed to run program: {e}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    let status = result?;
    Ok(exit_code_of(status))
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

fn report(diags: &[Diagnostic], path: &str, src: &str) -> ExitCode {
    eprint!("{}", diagnostics::render(diags, path, src, color_enabled()));
    let n = diags.len();
    let plural = if n == 1 { "error" } else { "errors" };
    eprintln!("{} could not compile `{path}` due to {n} previous {plural}", red("error:"));
    ExitCode::from(1)
}
