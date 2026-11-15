# The `fs` module

[← Back to the README](../README.md) · [Language guide](language.md)

Reading and writing files, working with folders, and taking paths apart.

```python
import fs

fn main():
    fs.write("notes.txt", "first line\n")
    fs.append("notes.txt", "second line\n")
    for line in fs.read_lines("notes.txt"):
        print(line)

    val config = fs.read_or_null("config.txt") ?: "defaults"
    fs.make_dir("out/reports")                    # and any missing parents
    for name in fs.list_dir("."):
        if fs.extension(name) == "tr":
            print(f"{fs.stem(name)} is a Tarn file")
```

Paths are `String`s, relative to the current directory unless they start with `/`. Text is read and written exactly as it is (UTF-8, with no newline conversion). The module is [written in Tarn](../stdlib/fs.tr), on top of a few C hooks.

## When something goes wrong

A file that can't be read or written (it doesn't exist, it's a folder, there's no permission...) is a **panic** with the reason from the operating system. Like every standard-library panic, it points at *your* line:

```
panic at app.tr:3:16: cannot read data.txt: No such file or directory
```

When a missing file is expected, check first with `fs.exists(path)`, or use `fs.read_or_null(path)`, which gives `null` instead of a panic.

## Files

| Function | |
|---|---|
| `fs.read(path) -> String` | The whole file |
| `fs.read_or_null(path) -> String?` | The whole file, or `null` if it can't be read |
| `fs.read_lines(path) -> List[String]` | The file's lines, without their `\n` or `\r\n` |
| `fs.write(path, text)` | Replace the file's contents (creating it if needed) |
| `fs.append(path, text)` | Add to the end of the file (creating it if needed) |
| `fs.read_stdin() -> String` | Everything left on standard input |

## Files and folders

| Function | |
|---|---|
| `fs.exists(path) -> Bool` | Is there a file or folder at `path`? |
| `fs.is_file(path)`, `fs.is_dir(path)` | `-> Bool` |
| `fs.list_dir(path) -> List[String]` | The names in a folder, sorted (without `.` and `..`) |
| `fs.make_dir(path)` | Create a folder and any missing folders above it. An existing folder is fine |
| `fs.remove(path)` | Delete a file, or an empty folder |
| `fs.rename(from, to)` | Move or rename |
| `fs.cwd() -> String` | The current directory |

## Paths

These only look at the text of the path; the file doesn't have to exist.

| Function | `"src/main.tr"` gives |
|---|---|
| `fs.join(a, b)` | `fs.join("src", "main.tr")` is `"src/main.tr"`. If `b` starts with `/`, it's `b` |
| `fs.file_name(path)` | `"main.tr"` |
| `fs.parent(path)` | `"src"` (and `""` for `"main.tr"`) |
| `fs.extension(path)` | `"tr"` (without the dot; `""` for `"Makefile"` and `".bashrc"`) |
| `fs.stem(path)` | `"main"` |
