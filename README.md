# RustArc

Small WinRAR-style archive manager (egui GUI) for Linux.

| Format | List | Extract | Test | Create |
|--------|------|---------|------|--------|
| 7z | yes | yes | yes | yes |
| RAR | yes | yes | yes | no (proprietary) |
| ZIP (also .jar/.apk/.cbz) | yes | yes (AES/ZipCrypto passwords) | yes | yes |
| TAR, TAR.GZ/.tgz, TAR.BZ2, TAR.XZ, TAR.ZST | yes | yes | yes | yes |
| single-file .gz .bz2 .xz .zst | yes | yes | yes | no |

When creating, the extension you type decides the format (default .7z).

Features: drag & drop, filter box, multi-select (Ctrl / Shift), password field,
selective extraction, zip-slip protection, background worker thread.

## Build the AppImage
    sudo apt install build-essential curl      # g++ is needed for the bundled UnRAR
    curl https://sh.rustup.rs -sSf | sh
    ./build-appimage.sh                        # -> RustArc-x86_64.AppImage

Or push to GitHub: `.github/workflows/build.yml` builds it on Ubuntu 22.04 and
uploads it as a workflow artifact.

Dev run: `cargo run --release -- some.7z`
