use super::aur::AurHelper;
use anyhow::anyhow;
use byte_unit::Byte;
use std::{fmt, path::PathBuf, str::FromStr};

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use super::presets::PresetsPath;

fn parse_bytes(src: &str) -> anyhow::Result<Byte> {
    if let Ok(val) = src.parse::<u128>() {
        let mib_in_bytes = val * 1024 * 1024;
        return Byte::from_u128(mib_in_bytes).ok_or_else(|| {
            anyhow!(
                "Invalid image size: raw number {} is too large to represent as bytes",
                val
            )
        });
    }
    Byte::parse_str(src, true).map_err(|e| anyhow!("Invalid image size, error: {:?}", e))
}

fn parse_presets_path(path: &str) -> anyhow::Result<PresetsPath> {
    PresetsPath::from_str(path).map_err(|e| anyhow!("{}", e))
}

#[derive(Parser, Debug, Clone)]
#[clap(name = "alma", about = "Arch Linux Mobile Appliance", version, author)]
pub struct App {
    /// Verbose output
    #[clap(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[clap(subcommand)]
    pub cmd: Command,
}

#[derive(Parser, Debug, Clone)]
pub enum Command {
    #[clap(name = "create", about = "Create a new Arch Linux bootable system")]
    Create(CreateCommand),
    #[clap(name = "install", about = "Install this system to another disk")]
    Install(InstallCommand),
    #[clap(name = "chroot", about = "Chroot into an existing ALMA system")]
    Chroot(ChrootCommand),
    #[clap(name = "qemu", about = "Boot the ALMA system with Qemu")]
    Qemu(QemuCommand),
}

#[derive(ValueEnum, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SystemVariant {
    #[default]
    Arch,
    Omarchy,
}

impl fmt::Display for SystemVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                SystemVariant::Arch => "arch",
                SystemVariant::Omarchy => "omarchy",
            }
        )
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemTypeArg {
    #[default]
    Ext4,
    Btrfs,
    Vfat,
}

#[derive(Parser, Debug, Clone)]
pub struct CreateCommand {
    /// Path to a block device or a non-existing file if --image is specified
    #[clap(value_name = "BLOCK_DEVICE | IMAGE")]
    pub path: Option<PathBuf>,

    /// The Linux system variant to install
    #[clap(long, value_enum, default_value_t = SystemVariant::Arch)]
    pub system: SystemVariant,

    /// The filesystem to use for the root partition
    #[clap(long, value_enum, default_value_t = FilesystemTypeArg::Ext4)]
    pub filesystem: FilesystemTypeArg,

    /// Path to a partition to use as the target root partition
    #[clap(long = "root-partition", value_name = "ROOT_PARTITION_PATH")]
    pub root_partition: Option<PathBuf>,

    /// Path to a partition to use as the target boot partition
    #[clap(long = "boot-partition", value_name = "BOOT_PARTITION_PATH")]
    pub boot_partition: Option<PathBuf>,

    /// Path to a pacman.conf file to use
    #[clap(short = 'c', long = "pacman-conf", value_name = "PACMAN_CONF")]
    pub pacman_conf: Option<PathBuf>,

    /// Additional packages to install from Pacman repos
    #[clap(short = 'p', long = "extra-packages", value_name = "PACKAGE")]
    pub extra_packages: Vec<String>,

    /// Additional packages to install from the AUR
    #[clap(long = "aur-packages", value_name = "AUR_PACKAGE")]
    pub aur_packages: Vec<String>,

    /// Boot partition size. Raw numbers are treated as MiB. [default: 300MiB]
    #[clap(long = "boot-size", value_name = "SIZE_WITH_UNIT", value_parser = parse_bytes)]
    pub boot_size: Option<Byte>,

    /// Enter interactive chroot before unmounting the drive
    #[clap(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Encrypt the root partition (highly recommended for Omarchy)
    #[clap(short = 'e', long = "encrypted-root")]
    pub encrypted_root: bool,

    /// Paths to preset files/dirs (local, http(s) zip/tar.gz, or git repo)
    #[clap(long = "presets", value_name = "PRESETS_PATH", value_parser = parse_presets_path)]
    pub presets: Vec<PresetsPath>,

    /// Create a raw image file instead of using a block device
    #[clap(long = "image", value_name = "SIZE_WITH_UNIT", requires = "path", value_parser = parse_bytes)]
    pub image: Option<Byte>,

    /// Overwrite existing image files. Use with caution!
    #[clap(long = "overwrite")]
    pub overwrite: bool,

    /// Allow installation on non-removable devices. Use with extreme caution!
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,

    /// The AUR helper to install for handling AUR packages.
    #[clap(long = "aur-helper", value_enum, default_value_t = AurHelper::Paru, ignore_case = true)]
    pub aur_helper: AurHelper,

    /// Do not ask for confirmation (not supported for Omarchy or encryption)
    #[clap(long = "noconfirm")]
    pub noconfirm: bool,

    /// Print commands instead of executing them
    #[clap(long = "dryrun")]
    pub dryrun: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct InstallCommand {
    /// The target block device to install to. If not provided, you will be prompted.
    /// Incompatible with --root-partition.
    #[clap(value_name = "TARGET_BLOCK_DEVICE", conflicts_with_all = &["root_partition", "boot_partition"])]
    pub target_device: Option<PathBuf>,

    /// Path to a pre-existing partition to use as the root filesystem.
    /// This is for installing alongside other OSes (e.g., Windows).
    #[clap(
        long = "root-partition",
        value_name = "ROOT_PARTITION_PATH",
        requires = "boot_partition"
    )]
    pub root_partition: Option<PathBuf>,

    /// Path to a pre-existing EFI partition to use for the bootloader.
    #[clap(
        long = "boot-partition",
        value_name = "BOOT_PARTITION_PATH",
        requires = "root_partition"
    )]
    pub boot_partition: Option<PathBuf>,

    /// Allow installation on non-removable devices. Use with extreme caution!
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,

    /// Do not ask for confirmation for any steps
    #[clap(long = "noconfirm")]
    pub noconfirm: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct ChrootCommand {
    /// Path to the ALMA system's block device or image file
    #[clap()]
    pub block_device: PathBuf,
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,
    #[clap()]
    pub command: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct QemuCommand {
    /// Path to the ALMA system's block device or image file
    #[clap()]
    pub block_device: PathBuf,
    /// Arguments to pass to qemu
    #[clap()]
    pub args: Vec<String>,
}

// Structs for the manifest file
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub alma_version: String,
    pub system_variant: SystemVariant,
    pub filesystem: FilesystemTypeArg,
    pub encrypted_root: bool,
    pub aur_helper: String,
    pub original_command: String,
    pub sources: Vec<Source>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    pub r#type: String,      // "preset" or "system"
    pub origin: String,      // URL or original local path
    pub baked_path: PathBuf, // Path inside the image
}
