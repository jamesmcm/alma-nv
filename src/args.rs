use super::aur::AurHelper;
use anyhow::anyhow;
use byte_unit::Byte;
use std::path::PathBuf;

use clap::Parser;

/// Parse size argument as bytes e.g. 10GB, 10GiB, etc.
/// Note b is treated as bytes not bits
fn parse_bytes(src: &str) -> anyhow::Result<Byte> {
    Byte::parse_str(src, true).map_err(|e| anyhow!("Invalid image size, error: {:?}", e))
}

#[derive(Parser)]
#[clap(name = "alma", about = "Arch Linux Mobile Appliance", version, author)]
pub struct App {
    /// Verbose output
    #[structopt(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[structopt(subcommand)]
    pub cmd: Command,
}

#[derive(Parser)]
pub enum Command {
    #[clap(name = "create", about = "Create a new Arch Linux USB")]
    Create(CreateCommand),

    #[clap(name = "chroot", about = "Chroot into exiting Live USB")]
    Chroot(ChrootCommand),

    #[clap(name = "qemu", about = "Boot the USB with Qemu")]
    Qemu(QemuCommand),
}

#[derive(Parser)]
pub struct CreateCommand {
    /// Either a path to a removable block device or a nonexisting file if --image is specified
    #[clap()]
    pub path: Option<PathBuf>, // TODO: Why is this optional?

    /// Path to a pacman.conf file which will be used to pacstrap packages into the image.
    ///
    /// This pacman.conf will also be copied into the resulting Arch Linux image.
    #[clap(short = 'c', long = "pacman-conf", value_name = "pacman_conf")]
    pub pacman_conf: Option<PathBuf>,

    /// Additional packages to install from Pacman repos
    #[clap(short = 'p', long = "extra-packages", value_name = "package")]
    pub extra_packages: Vec<String>,

    /// Additional packages to install from the AUR
    #[clap(long = "aur-packages", value_name = "aurpackage")]
    pub aur_packages: Vec<String>,

    /// Boot partition size in megabytes
    #[clap(long = "boot-size")]
    pub boot_size: Option<u32>,

    /// Enter interactive chroot before unmounting the drive
    #[clap(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Encrypt the root partition
    #[clap(short = 'e', long = "encrypted-root")]
    pub encrypted_root: bool,

    /// Path to preset files
    #[clap(long = "presets", value_name = "preset")]
    pub presets: Vec<PathBuf>,

    /// Create an image with a certain size in the given path instead of using an actual block device
    #[clap(long = "image", value_name = "size", requires = "path")]
    pub image: Option<Byte>, // TODO: Check parsing

    /// Overwrite existing image files. Use with caution!
    #[clap(long = "overwrite")]
    pub overwrite: bool,

    /// Allow installation on non-removable devices. Use with extreme caution!
    ///
    /// If no device is specified in the command line, the device selection menu will
    /// show non-removable devices
    #[clap(long = "allow-non-removable")]
    pub allow_non_removable: bool,

    #[clap(
        value_enum,
        long = "aur-helper",
        default_value = "paru",
        ignore_case = true
    )]
    pub aur_helper: AurHelper,
}

#[derive(Parser)]
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

#[derive(Parser)]
pub struct QemuCommand {
    /// Path starting with /dev/disk/by-id for the USB drive
    #[clap()]
    pub block_device: PathBuf,

    /// Arguments to pass to qemu
    #[clap()]
    pub args: Vec<String>,
}
