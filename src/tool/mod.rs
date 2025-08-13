mod chroot;
mod mount;
mod qemu;

use anyhow::{Context, anyhow};
pub use chroot::chroot;
pub use mount::mount;
pub use qemu::qemu;

use std::path::PathBuf;
use std::process::Command;
use which::which;

#[derive(Debug)]
pub struct Tool {
    pub exec: PathBuf,
    pub dryrun: bool,
}

impl Tool {
    pub fn find(name: &'static str, dryrun: bool) -> anyhow::Result<Self> {
        Ok(Self {
            exec: which(name).context(format!("Cannot find {name}"))?,
            dryrun,
        })
    }

    pub fn execute(&self) -> Command {
        Command::new(&self.exec)
    }
}

use crate::args::{CreateCommand, FilesystemTypeArg};

pub struct Tools {
    pub sgdisk: Tool,
    pub pacstrap: Tool,
    pub arch_chroot: Tool,
    pub genfstab: Tool,
    pub mkfat: Tool,
    pub mkext4: Tool,
    pub mkbtrfs: Option<Tool>,
    pub btrfs: Option<Tool>,
    pub git: Tool,
    pub cryptsetup: Option<Tool>,
    pub blkid: Option<Tool>,
}

impl Tools {
    pub fn new(command: &CreateCommand) -> anyhow::Result<Self> {
        let dryrun = command.dryrun;
        let encrypted = command.encrypted_root;
        let is_btrfs = matches!(command.filesystem, FilesystemTypeArg::Btrfs);

        Ok(Self {
            sgdisk: Tool::find("sgdisk", dryrun).map_err(|_| {
                anyhow!("sgdisk is required for partitioning the disk. Please install the 'gptfdisk' package.")
            })?,
            pacstrap: Tool::find("pacstrap", dryrun).map_err(|_| {
                anyhow!("pacstrap is required for installing the base system. Please install the 'arch-install-scripts' package.")
            })?,
            arch_chroot: Tool::find("arch-chroot", dryrun).map_err(|_| {
                anyhow!("arch-chroot is required for changing root into the new system. Please install the 'arch-install-scripts' package.")
            })?,
            genfstab: Tool::find("genfstab", dryrun).map_err(|_| {
                anyhow!("genfstab is required for generating fstab. Please install the 'arch-install-scripts' package.")
            })?,
            mkfat: Tool::find("mkfs.fat", dryrun).map_err(|_| {
                anyhow!("mkfs.fat is required for creating FAT filesystems. Please install the 'dosfstools' package.")
            })?,
            // TODO: Technically don't need ext4 if only using btrfs
            mkext4: Tool::find("mkfs.ext4", dryrun).map_err(|_| {
                anyhow!("mkfs.ext4 is required for creating ext4 filesystems. Please install the 'e2fsprogs' package.")
            })?,
            mkbtrfs: if is_btrfs {
                Some(Tool::find("mkfs.btrfs", dryrun).map_err(|_| {
                anyhow!("mkfs.btrfs is required for creating btrfs filesystems. Please install the 'btrfs-progs' package.")
            })?)
            } else {
                None
            },
            btrfs: if is_btrfs {
                Some(Tool::find("btrfs", dryrun).map_err(|_| {
                anyhow!("btrfs is required for creating btrfs filesystems. Please install the 'btrfs-progs' package.")
            })?)
            } else {
                None
            },
            git: Tool::find("git", dryrun).map_err(|_| {
                anyhow!("git is required for using ALMA. Please install the 'git' package.")
            })?,
            cryptsetup: if encrypted {
                Some(Tool::find("cryptsetup", dryrun).map_err(|_| {
                    anyhow!("cryptsetup is required for setting up encrypted filesystems. Please install the 'cryptsetup' package.")
                })?)
            } else {
                None
            },
            blkid: if encrypted {
                Some(Tool::find("blkid", dryrun).map_err(|_| {
                    anyhow!("blkid is required for setting up encrypted filesystems. Please install the 'util-linux' package.")
                })?)
            } else {
                None
            },
        })
    }
}
