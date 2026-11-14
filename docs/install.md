# Installing Isma

[← Back to the README](../README.md) · [Language guide](language.md)

## Requirements

- **Rust** (cargo), to build the compiler: <https://rustup.rs>.
- **clang**, which Isma uses to turn your programs into native code. Isma looks for it in `$ISMA_CC`, then on your `PATH`, then in `/usr/lib/llvm-*/bin`.
  - Debian/Ubuntu: `sudo apt install clang`
  - Fedora: `sudo dnf install clang`
  - Arch: `sudo pacman -S clang`
  - macOS: `xcode-select --install`

## Install

From the repository folder:

```bash
./install.sh            # interactive: [1] default or [2] custom
./install.sh -y         # default install, no questions asked
./install.sh --dir DIR  # install into DIR, no questions asked
```

- **Default** installs `isma` to `~/.local/bin`, adds that folder to your `PATH` if needed, and runs a self-test that compiles and runs a hello-world program.
- **Custom** asks for:
  - the folder: `~/.local/bin`, `/usr/local/bin`, `~/.cargo/bin` or any path you type;
  - whether to edit your `PATH`;
  - whether to run the self-test.
- **Folders that need root.** If the target folder needs root, the installer asks for your sudo password only to copy the binary. Don't run the script itself with `sudo`.
- **PATH changes** go into your shell config (`~/.bashrc`, `~/.zshrc`, `~/.profile` or a fish `conf.d` file) between `# >>> isma >>>` markers, so they can be removed cleanly.
- **Uninstaller copy.** The installer saves a copy of the uninstaller and an install record in `~/.local/share/isma/`.

`cargo install --path .` also works, and puts `isma` in `~/.cargo/bin`.

## Uninstall

```bash
./uninstall.sh                     # interactive: [1] wipe everything or [2] pick targets
./uninstall.sh -y                  # wipe everything, no questions asked
~/.local/share/isma/uninstall.sh   # the same script, still there if the repo is gone
```

The uninstaller finds every Isma binary, the compile cache, the `PATH` entries and the install record, and lists them as numbered targets. It only deletes things Isma created:
- a binary named `isma` is removed only if it really is Isma (it checks `--version`);
- a cache or data folder that contains anyone else's files is skipped;
- your source code is never touched.

## Options

| Option | Script | |
|---|---|---|
| `-y` | both | No questions: default install / wipe everything |
| `--dir DIR` | install | Install into `DIR` |
| `--no-path` | install | Never edit shell config files |
| `--no-test` | install | Skip the self-test |
| `--keep-cache` | uninstall | Leave `~/.cache/isma` alone |
| `--no-anim` | both | No animations |
| `NO_COLOR=1` | both | No colors |

Colors and animations also switch off automatically when the output isn't a terminal.

## The VS Code extension

The extension lives in its own project, `isma-vscode`. Build and install it with:

```bash
cd ../isma-vscode
npm install && npm run package
code --install-extension isma-*.vsix
```

It runs `isma lsp`, the language server built into the compiler. When you update `isma` with `./install.sh`, the extension notices the new binary and restarts the server by itself.
