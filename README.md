# ALMA Nueva - Arch Linux Mobile Appliance

ALMA Nueva (alma-nv) is a maintained and updated fork of [ALMA](https://github.com/r-darwish/alma) originally created by
[@r-darwish](https://github.com/r-darwish).

Almost every live Linux distribution out there is meant for a specific purpose, whether it's data
rescue, privacy, penetration testing or anything else. There are some more generic distributions
but all of them are based on squashfs, meaning that changes don't persist reboots.

ALMA is meant for those who wish to have a **mutable** live environment. It installs Arch
Linux into a USB or an SD card, almost as if it was a hard drive. Some configuration is applied in
order to minimize writes to the USB and making sure the system is bootable on both BIOS and UEFI
systems.

Upgrading your packages is as easy as running `pacman -Syu` while the system is booted. This tool also provides an easy chroot command, so you can keep your live environment up to date without having to boot it. Encrypting the root partition is as easy as providing the `-e` flag.

## Installation

You can either build the project using `cargo build --release` or install the `alma-nv`, `alma-nv-git` or `alma-nv-bin` package from the AUR.

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

### Installing to Pre-existing Partitions
ALMA can also install to a partition you've already created, which is useful for dual-booting or custom disk layouts.

```bash
# Install to /dev/sdX5, but DO NOT install a bootloader
sudo alma create --root-partition /dev/sdX5

# Install to /dev/sdX5 and install a bootloader to the EFI partition at /dev/sdX1
sudo alma create --root-partition /dev/sdX5 --boot-partition /dev/sdX1
```
**Warning:** The partition specified with `--root-partition` will be **reformatted to ext4**, deleting all its contents.

### Disk Encryption
You can enable full disk encryption (LUKS) for the root partition with the `-e` flag:
```bash
sudo alma create -e /dev/disk/by-id/usb-Generic_USB_Flash_Disk-0:0
```
You will be prompted to enter and confirm the encryption passphrase during image creation.

### Creating a Raw Image File
For development and testing, it can be useful to generate a raw image file instead of writing to a physical device.
```bash
# Create a 10GiB raw image file named almatest.img
sudo alma create --image 10GiB almatest.img
```

### Chrooting into an Installation
After the installation is done, you can `chroot` into the environment to perform further customizations before the first boot.
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
*   A list of packages to install: `packages = ["mypackage"]`
*   A list of AUR packages to install: `aur_packages = ["cool-app-git"]`
*   A post-installation script: `script = """ ... """`
*   Environment variables required by the preset: `environment_variables = ["USERNAME"]`
*   A list of shared directories from the host to be made available inside the chroot: `shared_directories = ["configs"]`

If a directory is provided, all `.toml` files within it are recursively crawled and executed in alphanumeric order. This allows you to structure complex installations, for example:
```
my_presets/
├── 00-add_user.toml
├── 10-xorg/
│   ├── 00-install.toml
│   └── 01-config.toml
└── 20-i3/
    ├── 00-install.toml
    └── 01-copy_dotfiles.toml
```

### Order of Execution
ALMA installs packages and runs preset scripts in the following order:
1.  All non-AUR packages from all presets are collected and installed in a single `pacstrap` command.
2.  If any preset requests AUR packages, an AUR helper (like `paru` or `yay`) is installed.
3.  All AUR packages from all presets are collected and installed using the AUR helper.
4.  Preset scripts are executed one by one, in the alphanumeric order of their filenames.

## Full Command-Line Reference
```
USAGE:
    alma create [OPTIONS] [BLOCK_DEVICE | IMAGE]

ARGS:
    <BLOCK_DEVICE | IMAGE>
            Either a path to a removable block device or a nonexisting file if --image is
            specified

OPTIONS:
    -h, --help
            Print help information

    -v, --verbose
            Verbose output

    --root-partition <ROOT_PARTITION_PATH>
            Path to a partition to use as the target root partition - this will reformat the
            partition to ext4

    --boot-partition <BOOT_PARTITION_PATH>
            Path to a partition to use as the target boot partition - this will reformat the
            partition to vfat and install GRUB

    -c, --pacman-conf <PACMAN_CONF>
            Path to a pacman.conf file which will be used to pacstrap packages into the image

    -p, --extra-packages <PACKAGE>...
            Additional packages to install from Pacman repos

    --aur-packages <AUR_PACKAGE>...
            Additional packages to install from the AUR

    --boot-size <SIZE_WITH_UNIT>
            Boot partition size. If a raw number is given, it is treated as MiB
            [default: 300MiB]

    -i, --interactive
            Enter interactive chroot before unmounting the drive

    -e, --encrypted-root
            Encrypt the root partition

    --presets <PRESETS_PATH>...
            Paths to preset files or directories (local, http(s) zip/tar.gz, or git repository)

    --image <SIZE_WITH_UNIT>
            Create an image with a certain size in the given path instead of using an actual
            block device

    --overwrite
            Overwrite existing image files. Use with caution!

    --allow-non-removable
            Allow installation on non-removable devices. Use with extreme caution!

    --aur-helper <AUR_HELPER>
            The AUR helper to install for handling AUR packages
            [default: paru] [possible values: paru, yay]

    --noconfirm
            Do not ask for confirmation for any steps (for non-interactive use)

    --dryrun
            Do not run any commands, just print them to stdfout
```

## Troubleshooting
### mkinitcpio: /etc/mkinitcpio.d/linux.preset: No such file or directory
Ensure you have both the `linux` and `base` packages installed on your host system.

### losetup: cannot find an unused loop device
Check that you are running ALMA with `sudo` privileges, and reboot if you have installed a kernel update since your last reboot.

### Problem opening /dev/... for reading! Error is 123.
This can sometimes happen on disks with unusual partition tables. Delete all partitions on the disk first (e.g., with `gparted` or `fdisk`) and try again.

## Similar Projects
* [NomadBSD](http://nomadbsd.org/)

## Useful Resources
* [Arch Wiki: Installing Arch Linux on a USB key](https://wiki.archlinux.org/index.php/Install_Arch_Linux_on_a_USB_key)
