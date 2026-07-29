# File System Backup CLI
<!-- markdownlint-disable MD013 -->

<!-- Badge style inspired by https://github.com/dnaka91/advent-of-code/blob/de37024ba3b385694e14f79c849370c0f605f054/README.md -->

<!-- [![Build Status][build-img]][build-url] -->
[![Documentation][doc-img]][doc-url]

<!--
[build-img]: https://img.shields.io/github/actions/workflow/status/Lej77/file_system_backup/ci.yml?branch=main&style=for-the-badge
[build-url]: https://github.com/Lej77/file_system_backup/actions/workflows/ci.yml
 -->
<!-- https://shields.io/badges/static-badge -->
[doc-img]: https://img.shields.io/badge/docs.rs-file_system_backup-4d76ae?style=for-the-badge
[doc-url]: https://lej77.github.io/file_system_backup

This repository contains a CLI tool for saving a snapshot/backup of file system information, i.e. file paths, file sizes and modification times. This information can be useful to track how disk usage changes over time or to know what programs were installed on a now lost disk.

This program usually create backups and also visualize them using disk space analyzers such as:

- [WizTree](https://www.diskanalyzer.com/) for Windows.
- [QDirStat](https://github.com/shundhammer/qdirstat) for Linux.
- [eDirStat](https://github.com/xangelix/edirstat) which is cross-platform.

This CLI also has inbuilt support for some operations in order to function without the above 3rd party programs.

## Creating backups

- On Windows the `wiz-tree-backup` command can be used to collect file information using `WizTree`.
  - `WizTree` will export the information into a temporary file that is uncompressed and can get quite large (expect up to 500 MB if there are many files and maybe more).
    - The temporary file will be read and compressed by this program before it is written to the final output location (file/stdout).
    - A "fake" filesystem can be used to store the temporary file from `WizTree` to minimize disk wear. Currently requires that [`WinFsp`](https://github.com/winfsp/winfsp) is installed and its DLL available in `PATH`.
  - `WizTree` is found by looking for common install locations.
  - Graceful cleanup: `WizTree` will be killed and any temporary files will be removed if the command is interrupted.

- The `backup` command works without any 3rd party program and will manually scan the file system to gather the required information.
  - Warning: this is not very well tested and have many bugs.
  - This command supports fast scanning of NTFS drives using MFT on both Windows and Linux.

- The `embedded-e-dir-stat-backup` command works without 3rd party programs by embedding the code from a [fork](https://github.com/Lej77/edirstat/tree/extra) of the [`eDirStat` project](github.com/xangelix/edirstat) inside this executable.
  - Since it reuses the snapshot creation code this command should be just as good at file indexing as `eDirStat` itself.
  - This command supports fast scanning of NTFS drives using MFT on both Windows and Linux.

### Fast scanning of NTFS drives using MFT

Some commands supports scanning the [MFT](https://en.wikipedia.org/wiki/NTFS#Master_File_Table) of NTFS disks for faster file indexing if permissions allow.
MFT scanning is a [standout feature of `WizTree`](https://www.diskanalyzer.com/wiztree-vs-windirstat), so there is more information there.

To preform a fast scan you need to grant the program access to the MFT data:

- On Windows this requires running the program with admin rights.
- On Linux you can allow any program to read (but not write) the MFT data by configuring permission for NTFS drives. This makes a lot of sense if the NTFS drives are already mounted so that anyone can read from them.
    1. `sudo nano /etc/udev/rules.d/99-ntfs-mft-read.rules`
    2. Add lines such as:\
        `ENV{ID_FS_UUID}=="XXXXXXXXXXXXXXXX", MODE="0664"`\
        with each NTFS disk's UUID.
    3. `sudo udevadm control --reload-rules`
    4. `sudo udevadm trigger --subsystem-match=block`
    5. Check the permission `ls -l /dev/sda2`
- On Linux running the program with `sudo` is also good enough to allow reading NTFS block devices.
- On Linux you can add your user to the `disk` group to access NTFS block devices. Though this would also allow other user programs to read and write to block devices.
- On Linux `sudo setcap cap_dac_read_search+ep /path/to/program` should allow access to block devices for only a specific program. (This has not been tested yet.)

## Visualizing backups

- On Windows the `wiz-tree-open` command can be used to import and visualize a backup using `WizTree`.
  - `WizTree` only supports uncompressed backups so if the input is compressed then a temporary file is written with the decompressed data.
  - `WizTree` is found by looking for common install locations.
  - Graceful cleanup: `WizTree` will be killed and any temporary files will be removed if the command is interrupted.

- On Linux the `q-dir-stat-open` command can be used to import and visualize a backup using `QDirStat`.
  - `QDirStat` imports "cache" files with the file extension `.cache` or `.cache.gz` so a compressed temporary file is written to disk.

- The `mount` command can read a backup file and create a "fake" filesystem that can be browsed to navigate the stored information.
  - Currently the only mount option is a [`WebDAV`](WebDAV) filesystem. This has inbuilt support on at least Windows.

## Platform support

Currently only Windows and Linux are tested and some commands/features are platform specific.

## Usage

[Download precompiled executables from GitHub releases ⬇️](https://github.com/Lej77/file_system_backup/releases)

To build from source you can clone the repo and compile using [`Cargo`](https://www.rust-lang.org/tools/install):

```bash
cargo run --release -- --help
```

### `cargo install`

You can use `cargo install` to easily build from source without manually cloning the repo:

```bash
cargo install --git https://github.com/Lej77/file_system_backup.git
```

You can use [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) to easily download the precompiled executables from a GitHub release:

```bash
cargo binstall --git https://github.com/Lej77/file_system_backup.git file_system_backup
```

After installing you can update the program using [nabijaczleweli/cargo-update: A cargo subcommand for checking and applying updates to installed executables](https://github.com/nabijaczleweli/cargo-update):

```bash
cargo install-update --git file_system_backup

# OR update all installed programs:
cargo install-update --git --all
```

You can uninstall uisng:

```bash
 cargo uninstall file_system_backup
```

## License

This project is released under either:

- [MIT License](./LICENSE-MIT)
- [Apache License (Version 2.0)](./LICENSE-APACHE)

at your choosing.

Note that some optional dependencies might be under different licenses.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
