Fixtures for the package tests in `tests/lang.rs` (`packages_installed_by_thorn`).

- `pkgs/`: three packages laid out as Thorn unpacks them (`thorn.toml`, `src/<name>.tr`).
- `app/`: a project whose `.thorn/packages.json` (written by hand here, by Thorn
  in real projects) points at them. `thorn.lock` also names `later`, which isn't
  installed, to test the "run `thorn sync`" hint.
