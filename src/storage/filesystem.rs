use super::markers::BlockDevice;
use crate::{args::RootFilesystemType, process::CommandExt, tool::Tool};
use anyhow::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemType {
    Ext4,
    Btrfs,
    Vfat,
}

impl From<RootFilesystemType> for FilesystemType {
    fn from(fs: RootFilesystemType) -> Self {
        match fs {
            RootFilesystemType::Ext4 => FilesystemType::Ext4,
            RootFilesystemType::Btrfs => FilesystemType::Btrfs,
        }
    }
}

impl FilesystemType {
    pub fn to_mount_type(self) -> &'static str {
        match self {
            FilesystemType::Ext4 => "ext4",
            FilesystemType::Btrfs => "btrfs",
            FilesystemType::Vfat => "vfat",
        }
    }
}

#[derive(Debug)]
pub struct Filesystem<'a> {
    fs_type: FilesystemType,
    block: &'a dyn BlockDevice,
}

impl<'a> Filesystem<'a> {
    pub fn format(
        block: &'a dyn BlockDevice,
        fs_type: FilesystemType,
        mkfs: &Tool,
    ) -> anyhow::Result<Self> {
        let mut command = mkfs.execute();
        match fs_type {
            FilesystemType::Ext4 => command.arg("-F").arg(block.path()),
            FilesystemType::Btrfs => command.arg("-f").arg(block.path()),
            FilesystemType::Vfat => command.arg("-F32").arg(block.path()),
        };

        command.run(mkfs.dryrun).with_context(|| {
            format!(
                "Error formatting {:?} with {}",
                fs_type,
                mkfs.exec.display()
            )
        })?;

        Ok(Self { fs_type, block })
    }

    pub fn from_partition(block: &'a dyn BlockDevice, fs_type: FilesystemType) -> Self {
        Self { fs_type, block }
    }

    pub fn block(&self) -> &dyn BlockDevice {
        self.block
    }

    pub fn fs_type(&self) -> FilesystemType {
        self.fs_type
    }
}
