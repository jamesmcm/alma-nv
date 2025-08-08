use super::markers::BlockDevice;
use crate::{args::FilesystemTypeArg, process::CommandExt, tool::Tool};
use anyhow::Context;

impl FilesystemTypeArg {
    pub fn to_mount_type(self) -> &'static str {
        match self {
            FilesystemTypeArg::Ext4 => "ext4",
            FilesystemTypeArg::Btrfs => "btrfs",
        }
    }
}

#[derive(Debug)]
pub struct Filesystem<'a> {
    fs_type: FilesystemTypeArg,
    block: &'a dyn BlockDevice,
}

impl<'a> Filesystem<'a> {
    pub fn format(
        block: &'a dyn BlockDevice,
        fs_type: FilesystemTypeArg,
        mkfs: &Tool,
    ) -> anyhow::Result<Self> {
        let mut command = mkfs.execute();
        match fs_type {
            FilesystemTypeArg::Ext4 => command.arg("-F").arg(block.path()),
            FilesystemTypeArg::Btrfs => command.arg("-f").arg(block.path()),
        };

        command
            .run(mkfs.dryrun)
            .with_context(|| format!("Error formatting filesystem with {}", mkfs.exec.display()))?;

        Ok(Self { fs_type, block })
    }

    pub fn from_partition(block: &'a dyn BlockDevice, fs_type: FilesystemTypeArg) -> Self {
        Self { fs_type, block }
    }

    pub fn block(&self) -> &dyn BlockDevice {
        self.block
    }

    pub fn fs_type(&self) -> FilesystemTypeArg {
        self.fs_type
    }
}
