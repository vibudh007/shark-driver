# Shark Driver

Shark Driver is an independent Linux configuration tool for Attack Shark mice. Use the GTK4 desktop app for everyday adjustments or the Rust CLI for scripts and diagnostics. The app detects supported mice connected by USB, 2.4 GHz receiver, or Bluetooth and shows the controls it can use on the selected connection.

## Features

- Adjust sensitivity stages, polling rate, button assignments, lighting, sleep and other settings where the mouse exposes them.
- Save profiles, edit or record macros, and import supported Windows profile and macro files.
- Check battery and pointer activity where available.
- Read back changes on supported settings paths and verify compatible macro backups byte for byte.
- Use the same app and command set in the [AppImage and Flatpak releases](https://github.com/vibudh007/shark-driver/releases/latest).

## Installation

### AppImage

Download the x86-64 AppImage from [Releases](https://github.com/vibudh007/shark-driver/releases/latest). It uses your system's GTK 4.12 or newer, Python 3.10 or newer with PyGObject, glibc 2.39 or newer, and libdbus.

```sh
chmod +x Shark-Driver-0.1.0-x86_64.AppImage
./Shark-Driver-0.1.0-x86_64.AppImage
```

If the FUSE mount fails, use extraction mode.

```sh
APPIMAGELAUNCHER_DISABLE=1 ./Shark-Driver-0.1.0-x86_64.AppImage --appimage-extract-and-run
```

The AppImage keeps profiles and macros in `.r1/` beside the image. Use `--cli list models` to list the recognized models or `--cli --help` for the CLI reference.

### Flatpak

Download the x86-64 Flatpak bundle. Flatpak gets the GNOME 49 runtime from Flathub.

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user ./Shark-Driver-0.1.0-x86_64.flatpak
flatpak run local.sharkdriver.SharkDriver
```

Launch it again from your app menu or use `flatpak run local.sharkdriver.SharkDriver --cli --help`. The Flatpak keeps its library and exports under `~/.var/app/local.sharkdriver.SharkDriver/data/shark-driver/`. It requests direct access to devices for USB configuration and input recording, access to BlueZ for Bluetooth, and a host command relay for the optional administrator helper.

### Device permissions

Shark Driver runs as your user. Device access depends on your system's permissions. If a protected operation needs administrator access, the GUI can use the optional Polkit helper. Build the CLI from this repository first. Then run these commands from the repository root if you want to install the helper and policy on your machine.

```sh
sudo install -m 0755 target/release/shark-driver-cli /usr/bin/shark-driver-cli
sudo install -m 0644 gui/packaging/local.sharkdriver.SharkDriver.policy \
  /usr/share/polkit-1/actions/local.sharkdriver.SharkDriver.policy
```

Restart the app after installing the helper. It can then request authorization for the session.

## Uninstallation

- **AppImage** — Delete the downloaded image. Keep the adjacent `.r1/` folder if you want to retain its library.
- **Flatpak** — Run `flatpak uninstall --user local.sharkdriver.SharkDriver`. The default uninstall keeps app data. Use `--delete-data` only if you also want to remove that local data.
- **Optional Polkit helper** — If you installed it using the commands above, remove `/usr/bin/shark-driver-cli` and `/usr/share/polkit-1/actions/local.sharkdriver.SharkDriver.policy` with administrator privileges.

## Limitations

- Model detection covers 18 models from the vendor's Windows utility. Feature support depends on model, firmware and connection. Fixture coverage is broader than physical testing.
- On the tested R1 firmware, Bluetooth can report battery through BlueZ but cannot read back settings or macros. Use USB or 2.4 GHz for configuration on that firmware. Eight other models currently have Bluetooth discovery and battery only.
- The audited R1 firmware has no verified way to restore an erased macro slot byte for byte. Persistent wired macro writes are blocked on the affected revision. High-rate FA65 receiver profile, button and macro operations also remain incomplete.
- The packaged app passed Wayland and X11 smoke tests and simulated settings checks over all three transports. The package tests did not exercise live mouse writes or a real Polkit prompt. Font and theme rendering can vary between the host GTK stack and the Flatpak runtime.

## Build from source

Use x86-64 Linux with Rust and Cargo 1.85 or newer, a C toolchain, `pkg-config`, and libdbus development headers. To run the native desktop build, install the GTK4, Python and PyGObject requirements listed above.

```sh
cargo build --release --locked --target-dir target
cargo build --release --locked --manifest-path gui/launcher/Cargo.toml --target-dir target/desktop
./target/release/shark-driver-cli --help
SHARK_DRIVER_DATA_ROOT="$PWD" ./target/desktop/release/shark-driver
```

Build the CLI first because the launcher embeds `target/release/shark-driver-cli`. The source uses the same embed for both package formats.

### Build the AppImage and Flatpak

Install Flatpak, ImageMagick, and the usual archive and checksum utilities. Prepare the pinned AppImage tools and the project-local GNOME runtime. The packaging script checks the AppImage tool and runtime hashes before use.

```sh
work="$PWD/.runtime/packages"
mkdir -p "$work"/bin "$work"/cache "$work"/config "$work"/data "$work"/tmp
curl -fL https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage \
  -o "$work/bin/appimagetool-1.9.1"
curl -fL https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64 \
  -o "$work/bin/runtime-x86_64"
chmod +x "$work/bin/appimagetool-1.9.1"

export FLATPAK_USER_DIR="$work/flatpak-user"
export XDG_CACHE_HOME="$work/cache" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data"
export TMPDIR="$work/tmp"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user --noninteractive --no-related -y flathub org.gnome.Platform//49
bash tools/package-desktop.sh
(cd dist && sha256sum -c SHA256SUMS)
```

The script writes the packages to `dist/`. The upstream `continuous` runtime asset can change. If its digest differs, use the recorded build or review and verify the new upstream runtime before changing the pin. [Pinned AppImage runtime source](https://github.com/AppImage/type2-runtime/tree/fedfad51acc2860b2916a1eef32ba4a500ff31ef)

## License

Shark Driver is licensed under the [GNU General Public License, version 3 only](LICENSE). [Third-party components and notices](THIRD_PARTY_NOTICES).
