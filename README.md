# ALMA Nueva - Arch Linux Mobile Appliance

ALMA Nueva (alma-nv) is a maintained and updated fork of [ALMA](https://github.com/r-darwish/alma) originally created by
[@r-darwish](https://github.com/r-darwish).

Almost every live Linux distribution out there is meant for a specific purpose, whether it's data
rescue, privacy, penetration testing or anything else. There are some more generic distributions
but all of them are based on squashfs, meaning that changes don't persist reboots.

ALMA is meant for those who wish to have a **mutable** live environment. It installs Arch
Linux into a USB, SD card, or image file, almost as if it was a hard drive. Configuration is applied
to minimize writes and ensure the system is bootable on both BIOS and UEFI systems.

Upgrading your packages is as easy as running `pacman -Syu`. This tool also provides an easy `chroot` command, so you can keep your live environment up to date without having to boot it. Encrypting the root partition is as easy as providing the `-e` flag.

## Installation

You can either build the project using `cargo build --release` or install the [alma-git](https://aur.archlinux.org/packages/alma-git) package from the AUR.

### Host system prerequisites

ALMA must be run on Arch Linux (derivatives are not supported). Install these packages on the host before running `alma`:

- `arch-install-scripts` (provides pacstrap, arch-chroot, genfstab)
- `gptfdisk` (provides sgdisk)
- `dosfstools` (provides mkfs.fat)
- `e2fsprogs` (provides mkfs.ext4)
- `btrfs-progs` (required for BTRFS support)
- `util-linux` (provides losetup, blkid, sfdisk; typically part of base)
- `git` (required for presets and AUR helper installation)
- `cryptsetup` (only required when using `--encrypted-root`)

Quick install:

```bash
sudo pacman -S --needed arch-install-scripts gptfdisk dosfstools e2fsprogs btrfs-progs util-linux git cryptsetup
```

Optional, for QEMU testing, see the QEMU section below.

### Using Docker (Cross-Platform)

ALMA can run on any system using Docker. This is useful for running ALMA on Fedora, macOS, or any other system with Docker installed.

#### Prerequisites

- Docker installed and running
- User in the `docker` group, or use `sudo`

#### How it works

The `run-alma.sh` script automatically:

- Builds the Docker image with all required Arch Linux tools
- Runs ALMA with proper privileges for device access
- Mounts the current directory as the working directory
- Handles all Docker complexity transparently

#### Usage Examples

**Linux/macOS (Bash):**

```bash
# Clone the repository
git clone https://github.com/assapir/alma-nv.git
cd alma-nv

# Make the run script executable
chmod +x run-alma.sh

# Create an encrypted 4GB image file
./run-alma.sh create -e --image 4GB my-arch-usb.img

# Create a bootable USB drive (requires sudo)
sudo ./run-alma.sh create /dev/sdb

# Chroot into an existing installation
sudo ./run-alma.sh chroot /dev/sdb
```

#### Security Note

The Docker container runs with `--privileged` access to perform disk operations. This is required for device access, partitioning, and filesystem operations, but has security implications. Only run ALMA containers from trusted sources.

### Using Arch Linux derivatives

Using Arch Linux derivatives, such as Manjaro, isn't supported by ALMA. It may work and may not. Please do not open bugs or feature
requests if you are not using an official Arch Linux installation as your host system.

## Usage

### Wiping a Device and Creating a New Installation

```bash
sudo alma create /dev/disk/by-id/usb-Generic_USB_Flash_Disk-0:0
```

This command will wipe the entire disk and create a fresh, bootable installation of Arch Linux. You can use either removable devices or loop devices. As a precaution, ALMA will not wipe non-removable devices unless you explicitly allow it with `--allow-non-removable`.

If you do not specify a device path, ALMA will interactively prompt you to select one from a list of available removable devices.

#### Interactive Setup

When you run `alma create` without presets or the `--noconfirm` flag, you will be guided through an interactive setup wizard. This allows you to configure a username, password, hostname, timezone, graphics drivers, and fonts for your new system.

### Installing to Another Disk (Cloning)

Once you have a booted ALMA system, you can use the `install` command to "clone" it to another disk. This re-runs the original creation process (using a manifest saved on the system) to create a fresh installation on the target device.

```bash
# From a running ALMA system, install to /dev/sdb
sudo alma install /dev/sdb

# Optionally, copy /home and NetworkManager configs from the running system
# (You will be prompted for this automatically)
```

### Installing to Pre-existing Partitions

ALMA can also install to a partition you've already created, which is useful for dual-booting or custom disk layouts.

```bash
# Install to /dev/sdX5, but DO NOT install a bootloader
sudo alma create --root-partition /dev/sdX5

# Install to /dev/sdX5 and install a bootloader to the EFI partition at /dev/sdX1
sudo alma create --root-partition /dev/sdX5 --boot-partition /dev/sdX1
```

**Warning:** The partition specified with `--root-partition` will be **reformatted**, deleting all its contents.

### System Variants and Filesystems

ALMA supports different system variants and root filesystems.

```bash
# Create an Omarchy system (defaults to BTRFS)
sudo alma create --system omarchy my-omarchy.img --image 8GiB

# Create a standard Arch system with a BTRFS filesystem
sudo alma create --filesystem btrfs my-btrfs.img --image 8GiB
```

- `--system`: `arch` (default) or `omarchy`.
- `--filesystem`: `ext4` (default) or `btrfs`.

### Disk Encryption

You can enable full disk encryption (LUKS) for the root partition with the `-e` flag:

```bash
sudo alma create -e /dev/disk/by-id/usb-Generic_USB_Flash_Disk-0:0
```

You will be prompted to enter and confirm the encryption passphrase during creation.

### Creating a Raw Image File

For development and testing, it can be useful to generate a raw image file instead of writing to a physical device.

```bash
# Create a 10GiB raw image file named almatest.img
sudo alma create --image 10GiB almatest.img
```

### Chrooting into an Installation

After the installation is done, you can `chroot` into the environment to perform further customizations before the first boot. ALMA will automatically detect partitions and filesystem types (ext4/btrfs/LUKS).

```bash
sudo alma chroot /dev/disk/by-id/usb-Generic_USB_Flash_Disk-0:0
```

### Booting in QEMU

You can easily boot a device or image file in QEMU for testing.

Note you will need to install `qemu-desktop`, `qemu-system-x86` and `qemu-system-x86-firmware`.

```bash
sudo pacman -S qemu-desktop qemu-system-x86 qemu-system-x86-firmware
```

```bash
# First, mount a loop device for your image
sudo losetup -fP --show almatest.img
# It will print something like /dev/loop0

# Then boot it
sudo alma qemu /dev/loop0
```

## Presets

Reproducing a build can be easily done using preset files. Presets are powerful TOML files that let you define packages to install, scripts to run, and more.

You can specify presets from a local file, a directory, a remote URL, or a Git repository.

```bash
# Use a local preset file and a directory of presets
sudo ALMA_USER=archie alma create --presets ./user.toml ./presets/

# Clone a git repository over HTTPS and use its presets
sudo alma create --presets https://github.com/user/my-alma-presets.git

# Download and extract a zip file of presets
sudo alma create --presets https://example.com/presets.zip
```

Preset files are simple TOML files which contain:

- A list of packages to install: `packages = ["mypackage"]`
- A list of AUR packages to install: `aur_packages = ["cool-app-git"]`
- A post-installation script: `script = """ ... """`
- Environment variables required by the preset: `environment_variables = ["USERNAME"]`
- A list of shared directories from the host to be made available inside the chroot: `shared_directories = ["configs"]`

If a directory is provided, all `.toml` files within it are recursively crawled and executed in alphanumeric order. This allows you to structure complex installations.

### Order of Execution

ALMA installs packages and runs preset scripts in the following order:

1.  All non-AUR packages from all presets are collected and installed in a single `pacstrap` command.
2.  If any preset requests AUR packages, an AUR helper (like `paru` or `yay`) is installed.
3.  All AUR packages from all presets are collected and installed using the AUR helper.
4.  Preset scripts are executed one by one, in the alphanumeric order of their filenames.

## Full Command-Line Reference

<details>
<summary>Click to expand for all commands and options</summary>

```
alma 0.3.0
Arch Linux Mobile Appliance

USAGE:
    alma [OPTIONS] <SUBCOMMAND>

OPTIONS:
    -h, --help       Print help information
    -v, --verbose    Verbose output
    -V, --version    Print version information

SUBCOMMANDS:
    create     Create a new Arch Linux bootable system
    install    Install this system to another disk
    chroot     Chroot into an existing ALMA system
    qemu       Boot the ALMA system with Qemu
    help       Print this message or the help of the given subcommand(s)
```

**`alma create`**
```
USAGE:
    alma create [OPTIONS] [path]

ARGS:
    <path>    Path to a block device or a non-existing file if --image is specified

OPTIONS:
        --allow-non-removable
            Allow installation on non-removable devices. Use with extreme caution!

        --aur-helper <aur-helper>
            The AUR helper to install for handling AUR packages

            [default: paru]
            [possible values: paru, yay]

        --aur-packages <AUR_PACKAGE>
            Additional packages to install from the AUR

        --boot-partition <BOOT_PARTITION_PATH>
            Path to a partition to use as the target boot partition - this will reformat the
            partition to vfat and install GRUB. Should be used with --root-partition if you want to
            install a bootloader to a pre-partitioned disk. If --root-partition is set, but this is
            not, then no bootloader will be installed

        --boot-size <SIZE_WITH_UNIT>
            Boot partition size. Raw numbers are treated as MiB. [default: 300MiB]

        --dryrun
            Print commands instead of executing them

    -e, --encrypted-root
            Encrypt the root partition (highly recommended for Omarchy)

    -p, --extra-packages <PACKAGE>
            Additional packages to install from Pacman repos

        --filesystem <filesystem>
            The filesystem to use for the root partition

            [default: ext4]
            [possible values: ext4, btrfs]

    -h, --help
            Print help information

        --image <SIZE_WITH_UNIT>
            Create a raw image file instead of using a block device

    -i, --interactive
            Enter interactive chroot before unmounting the drive

        --noconfirm
            Do not ask for confirmation (not supported for Omarchy or encryption)

        --overwrite
            Overwrite existing image files. Use with caution!

    -c, --pacman-conf <PACMAN_CONF>
            Path to a pacman.conf file which will be used to pacstrap packages into the image. This
            pacman.conf will also be copied into the resulting Arch Linux image

        --presets <PRESETS_PATH>
            Paths to preset files/dirs (local, http(s) zip/tar.gz, or git repo)

        --root-partition <ROOT_PARTITION_PATH>
            Path to a partition to use as the target root partition - this will reformat the
            partition. Should be used when you do not want to repartition and wipe the entire disk
            (e.g. dual-booting). If it is not set, then the entire disk will be repartitioned and
            wiped. If it is set, but --boot-partition is not, then the partition will be mounted as /
            and /boot will not be modified

        --system <system>
            The Linux system variant to install

            [default: arch]
            [possible values: arch, omarchy]
```

**`alma install`**
```
USAGE:
    alma install [OPTIONS] [target_device]

ARGS:
    <target_device>
            The target block device to install to. If not provided, you will be prompted.
            Incompatible with --root-partition

OPTIONS:
        --allow-non-removable    Allow installation on non-removable devices. Use with extreme
                                 caution!
        --boot-partition <BOOT_PARTITION_PATH>
            Path to a pre-existing EFI partition to use for the bootloader
    -h, --help                   Print help information
        --noconfirm              Do not ask for confirmation for any steps
        --root-partition <ROOT_PARTITION_PATH>
            Path to a pre-existing partition to use as the root filesystem. This is for installing
            alongside other OSes (e.g., Windows)
```

</details>

## Troubleshooting

### mkinitcpio: /etc/mkinitcpio.d/linux.preset: No such file or directory

Ensure you have both the `linux` and `base` packages installed on your host system.

### losetup: cannot find an unused loop device

Check that you are running ALMA with `sudo` privileges, and reboot if you have installed a kernel update since your last reboot.

### Problem opening /dev/... for reading! Error is 123.

This can sometimes happen on disks with unusual partition tables. Delete all partitions on the disk first (e.g., with `gparted` or `fdisk`) and try again.

## Similar Projects

- [NomadBSD](http://nomadbsd.org/)

## Useful Resources

- [Arch Wiki: Installing Arch Linux on a USB key](https://wiki.archlinux.org/index.php/Install_Arch_Linux_on_a_USB_key)
```
