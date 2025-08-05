use super::aur::AurHelper;
use anyhow::anyhow;
use byte_unit::Byte;
use std::{path::PathBuf, str::FromStr};

use clap::Parser;

use super::presets::PresetsPath;

/// Parse size argument as bytes e.g. 10GB, 10GiB, etc.
/// If a raw number is given, it is treated as MiB.
fn parse_bytes(src: &str) -> anyhow::Result<Byte> {
    // If the input is just a number, treat it as MiB
    if let Ok(val) = src.parse::<u128>() {
        let mib_in_bytes = val * 1024 * 1024;
        return Byte::from_u128(mib_in_bytes).ok_or_else(|| {
            anyhow!(
                "Invalid image size: raw number {} is too large to represent as bytes",
                val
            )
        });
    }
    // Otherwise, parse it as a string with units (e.g., "500GiB")
    Byte::parse_str(src, true).map_err(|e| anyhow!("Invalid image size, error: {:?}", e))
}

fn parse_presets_path(path: &str) -> anyhow::Result<PresetsPath> {
    PresetsPath::from_str(path).map_err(|e| anyhow!("{}", e))
}

#[derive(Parser, Debug, Clone)]
#[clap(name = "alma", about = "Arch Linux Mobile Appliance", version, author)]
pub struct App {
    /// Verbose output
    #[structopt(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[structopt(subcommand)]
    pub cmd: Command,
}

#[derive(Parser, Debug, Clone)]
pub enum Command {
    #[clap(name = "create", about = "Create a new Arch Linux USB")]
    Create(CreateCommand),

    #[clap(name = "chroot", about = "Chroot into exiting Live USB")]
    Chroot(ChrootCommand),

    #[clap(name = "qemu", about = "Boot the USB with Qemu")]
    Qemu(QemuCommand),
}

#[derive(Parser, Debug, Clone)]
pub struct CreateCommand {
    /// Either a path to a removable block device or a nonexisting file if --image is specified
    #[clap(value_name = "BLOCK_DEVICE | IMAGE")]
    pub path: Option<PathBuf>, // If not present then user is prompted interactively

    /// Path to a partition to use as the target root partition - this will reformat the partition to ext4
    /// Should be used when you do not want to repartition and wipe the entire disk (e.g. dual-booting or install on to a disk with existing partitions)
    /// If it is not set, then the entire disk will be repartitioned and wiped
    /// If it is set, but --boot-partition is not, then the partition will be mounted as / and /boot will not be modified
    #[clap(long = "root-partition", value_name = "ROOT_PARTITION_PATH")]
    pub root_partition: Option<PathBuf>,

    // TODO: Add support for separate home partition too?
    /// Path to a partition to use as the target boot partition - this will reformat the partition to vfat and install GRUB.
    /// Should be used with --root-partition if you want to install a bootloader to a pre-partitioned disk.
    /// If --root-partition is set, but this is not, then no bootloader will be installed.
    #[clap(long = "boot-partition", value_name = "BOOT_PARTITION_PATH")]
    pub boot_partition: Option<PathBuf>,

    /// Path to a pacman.conf file which will be used to pacstrap packages into the image.
    ///
    /// This pacman.conf will also be copied into the resulting Arch Linux image.
    #[clap(short = 'c', long = "pacman-conf", value_name = "PACMAN_CONF")]
    pub pacman_conf: Option<PathBuf>,

    /// Additional packages to install from Pacman repos
    #[clap(short = 'p', long = "extra-packages", value_name = "PACKAGE")]
    pub extra_packages: Vec<String>,

    /// Additional packages to install from the AUR
    #[clap(long = "aur-packages", value_name = "AUR_PACKAGE")]
    pub aur_packages: Vec<String>,

    /// Boot partition size. If a raw number is given, it is treated as MiB.
    /// [default: 300MiB]
    #[clap(long = "boot-size", value_name = "SIZE_WITH_UNIT", value_parser = parse_bytes)]
    pub boot_size: Option<Byte>,

    /// Enter interactive chroot before unmounting the drive
    #[clap(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Encrypt the root partition
    #[clap(short = 'e', long = "encrypted-root")]
    pub encrypted_root: bool,

    /// Paths to preset files or directories (local, http(s) zip/tar.gz, or git repository)
    #[clap(long = "presets", value_name = "PRESETS_PATH", value_parser = parse_presets_path)]
    pub presets: Vec<PresetsPath>,

    /// Create an image with a certain size in the given path instead of using an actual block device
    #[clap(long = "image", value_name = "SIZE_WITH_UNIT", requires = "path", value_parser = parse_bytes)]
    pub image: Option<Byte>,

    /// Overwrite existing image files. Use with caution!
    #[clap(long = "overwrite")]
    pub overwrite: bool,

    /// Allow installation on non-removable devices. Use with extreme caution!
    ///
    /// If no device is specified in the command line, the device selection menu will
    /// show non-removable devices
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,

    /// The AUR helper to install for handling AUR packages.
    #[clap(
        value_enum,
        long = "aur-helper",
        default_value_t = AurHelper::Paru,
        ignore_case = true
    )]
    pub aur_helper: AurHelper,

    /// Do not ask for confirmation for any steps (for non-interactive use)
    #[clap(long = "noconfirm")]
    pub noconfirm: bool,

    /// Do not run any commands, just print them to stdfout
    #[clap(long = "dryrun")]
    pub dryrun: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct ChrootCommand {
    /// Path starting with /dev/disk/by-id for the USB drive
    #[clap()]
    pub block_device: PathBuf,

    /// Allow installation on non-removable devices. Use with extreme caution!
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,

    /// Optional command to run
    #[clap()]
    pub command: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct QemuCommand {
    /// Path starting with /dev/disk/by-id for the USB drive
    #[clap()]
    pub block_device: PathBuf,

    /// Arguments to pass to qemu
    #[clap()]
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byte_parsing() {
        let app_parse =
            App::try_parse_from(vec!["alma-nv", "create", "--image", "500MiB", "/path/test"]);
        match app_parse {
            Err(e) => {
                println!("{e}");
                panic!("arg parsing failed");
            }
            Ok(app) => {
                if let Command::Create(cmd) = app.cmd {
                    let path = PathBuf::from_str("/path/test").unwrap();

                    assert_eq!(
                        cmd.image,
                        Some(Byte::from_i128_with_unit(500, byte_unit::Unit::MiB).unwrap())
                    );
                    assert_eq!(cmd.path, Some(path));
                } else {
                    panic!("was not Create command")
                }
            }
        }
    }

    #[test]
    fn test_byte_parsing_case_insensitive() {
        let app_parse =
            App::try_parse_from(vec!["alma-nv", "create", "--image", "500mb", "/path/test"]);
        match app_parse {
            Err(e) => {
                println!("{e}");
                panic!("arg parsing failed");
            }
            Ok(app) => {
                if let Command::Create(cmd) = app.cmd {
                    let path = PathBuf::from_str("/path/test").unwrap();

                    assert_eq!(
                        cmd.image,
                        Some(Byte::from_i128_with_unit(500, byte_unit::Unit::MB).unwrap())
                    );
                    assert_eq!(cmd.path, Some(path));
                } else {
                    panic!("was not Create command")
                }
            }
        }
    }

    #[test]
    fn test_byte_parsing_no_unit() {
        let app_parse = App::try_parse_from(vec![
            "alma-nv",
            "create",
            "--boot-size",
            "500",
            "/path/test",
        ]);
        match app_parse {
            Err(e) => {
                println!("{e}");
                panic!("arg parsing failed");
            }
            Ok(app) => {
                if let Command::Create(cmd) = app.cmd {
                    assert_eq!(
                        cmd.boot_size,
                        Some(Byte::from_u128(500 * 1024 * 1024).unwrap())
                    );
                } else {
                    panic!("was not Create command")
                }
            }
        }
    }
}
