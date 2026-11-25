# DDO Launcher for Linux

A simple replacement for the Windows-only DDON launcher, running natively on Linux
with [umu-launcher](https://github.com/Open-Wine-Components/umu-launcher).

It also includes a "dummy" launcher executable which is a simple no-op executable. 
This can be used to replace the original `ddo_launcher.exe` to prevent the game
from launching that one on exit.

## Usage

Simply run `ddo_launcher_native` from anywhere. Inside the window, you can enter 
your account name and password, and configure the server you wish to play on.

You must also set `DDO.exe Path` and `Wine Prefix Path`. The wine prefix path can be set to 
any empty folder, `umu-launcher` will take care of initializing the prefix on first launch.

## Local Play

`Local Play` (or `ddo_launcher_native --local-play`) runs the game against a local
[Arrowgene.DragonsDogmaOnline](https://github.com/sebastian-heinz/Arrowgene.DragonsDogmaOnline) server,
the same way as the Local Play mode of [DDON-Launcher-WPF](https://github.com/D00MK1D/DDON-Launcher-WPF):

- The latest linux-x64 server release is installed into `LocalServer/Server` next to `DDO.exe` and updated
  when a newer release is out. `db.sqlite` and its `Backups` folder are kept across updates.
- `db.sqlite` is backed up, `dbmigration` runs, the server starts, and the launcher logs in as `LocalPlayer`
  (the account is created on first use).
- The server stops when the game exits. Its output goes to `LocalServer/server.log`.

A `LocalServer` folder made by DDON-Launcher-WPF under Proton is reused. Its `db.sqlite` carries over, and
the Windows server's config is kept as `Arrowgene.Ddon.config.other-platform.json` because it holds Windows paths.

The server runs with the system .NET 10 runtime (`aspnet-runtime` on Arch).

## Build

```bash
cargo build --package ddo_launcher_native --release
cargo build --package ddo_launcher_dummy --release --target i686-pc-windows-gnu

./target/release/ddo_launcher_native
```

### Dependencies

`rustup` must be installed and a toolchain set. It can be found at [rustup.rs](https://rustup.rs/)
or in your distribution's package manager.

In order to build the "dummy" launcher executable, a Windows target must also be installed.
On Arch-based distributions, use the following `pacman` command. On other distributions,
find and install the corresponding package.

```bash
sudo pacman -S mingw-w64-gcc
rustup target add i686-pc-windows-gnu
```
