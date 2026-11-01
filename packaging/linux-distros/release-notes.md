
## Fedora, openSUSE, Arch and AppImage

The same Linux programs, repackaged from the `.deb` for distributions that do not use it, for x86-64 and Arm64:

| | x86-64 | Arm64 (aarch64) |
|---|---|---|
| Fedora / RHEL / openSUSE | `diskrescue-@VERSION@-1.x86_64.rpm` | `diskrescue-@VERSION@-1.aarch64.rpm` |
| Arch / Manjaro / EndeavourOS | `diskrescue-@VERSION@-1-x86_64.pkg.tar.zst` | `diskrescue-@VERSION@-1-aarch64.pkg.tar.zst` |
| AppImage (desktop app) | `DiskRescue-@VERSION@-x86_64.AppImage` | `DiskRescue-@VERSION@-aarch64.AppImage` |
| AUR recipe (`diskrescue-bin`) | `diskrescue-bin-@VERSION@-aur.tar.gz` | (same file) |

- Fedora: `sudo dnf install ./diskrescue-@VERSION@-1.x86_64.rpm`; openSUSE: `sudo zypper install --allow-unsigned-rpm ./diskrescue-@VERSION@-1.x86_64.rpm` (not signed); Arch: `sudo pacman -U ./diskrescue-@VERSION@-1-x86_64.pkg.tar.zst`.
- They need glibc 2.39 or newer (Fedora 40+, openSUSE Tumbleweed, current Arch), like the `.deb`.
- The AppImage runs the desktop app on any such distribution. To copy a drive that needs administrator rights from it, start it with `--appimage-extract-and-run`: pkexec cannot reach a program inside a mounted AppImage.
