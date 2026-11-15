# Installing Tarn

[← Back to the README](../README.md) · [Language guide](language.md)

## Requirements

- **Rust** (cargo), to build the compiler: <https://rustup.rs>.
- **clang**, which Tarn uses to turn your programs into native code. Tarn looks for it in `$TARN_CC`, then on your `PATH`, then in `/usr/lib/llvm-*/bin`.
  - Debian/Ubuntu: `sudo apt install clang`
  - Fedora: `sudo dnf install clang`
  - Arch: `sudo pacman -S clang`
  - macOS: `xcode-select --install`

## Install

From the repository folder:

```bash
./install.sh            # interactive: default or custom
./install.sh -y         # default install, no questions asked
./install.sh --dir DIR  # install into DIR, no questions asked
```

- **Default** installs `tarn` to `~/.local/bin`, adds that folder to your `PATH` if needed, and runs a self-test that compiles and runs a hello-world program.
- **Custom** asks for:
  - the folder: `~/.local/bin`, `/usr/local/bin`, `~/.cargo/bin` or any path you type;
  - whether to edit your `PATH`;
  - whether to run the self-test.
- **Folders that need root.** If the target folder needs root, the installer asks for your sudo password only to copy the binary. Don't run the script itself with `sudo`.
- **PATH changes** go into your shell config (`~/.bashrc`, `~/.zshrc`, `~/.profile` or a fish `conf.d` file) between `# >>> tarn >>>` markers, so they can be removed cleanly.
- **Uninstaller copy.** The installer saves a copy of the uninstaller and an install record in `~/.local/share/tarn/`.

`cargo install --path .` also works, and puts `tarn` in `~/.cargo/bin`.

## Uninstall

```bash
./uninstall.sh                     # interactive: remove everything, or pick
./uninstall.sh -y                  # remove everything, no questions asked
~/.local/share/tarn/uninstall.sh   # the same script, still there if the repo is gone
```

The uninstaller finds every Tarn binary, the compile cache, the `PATH` entries and the install record, and lists them, numbered. It only deletes things Tarn created:
- a binary named `tarn` is removed only if it really is Tarn (it checks `--version`);
- a cache or data folder that contains anyone else's files is skipped;
- your source code is never touched.

## Options

| Option | Script | |
|---|---|---|
| `-y` | both | No questions: default install / remove everything |
| `--dir DIR` | install | Install into `DIR` |
| `--no-path` | install | Never edit shell config files |
| `--no-test` | install | Skip the self-test |
| `--keep-cache` | uninstall | Leave `~/.cache/tarn` alone |
| `--no-anim` | install | No spinner |
| `NO_COLOR=1` | both | No colors |

Colors and the spinner also switch off automatically when the output isn't a terminal.

## The VS Code extension

The extension lives in its own project, `tarn-vscode`. Build and install it with:

```bash
cd ../tarn-vscode
npm install && npm run package
code --install-extension tarn-*.vsix
```

It runs `tarn lsp`, the language server built into the compiler. When you update `tarn` with `./install.sh`, the extension notices the new binary and restarts the server by itself.
