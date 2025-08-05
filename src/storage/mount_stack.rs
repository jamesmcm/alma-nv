use super::Filesystem;
use anyhow::anyhow;
use log::{debug, warn};
use nix::mount::{MsFlags, mount, umount};
use std::marker::PhantomData;
use std::path::PathBuf;

pub struct MountStack<'a> {
    targets: Vec<PathBuf>,
    filesystems: PhantomData<Filesystem<'a>>,
    dryrun: bool,
}

impl<'a> MountStack<'a> {
    pub fn new(dryrun: bool) -> Self {
        MountStack {
            targets: Vec::new(),
            filesystems: PhantomData,
            dryrun,
        }
    }

    pub fn mount(
        &mut self,
        filesystem: &'a Filesystem,
        target: PathBuf,
        options: Option<&str>,
    ) -> nix::Result<()> {
        let source = filesystem.block().path();
        debug!("Mounting {filesystem:?} to {target:?}");
        if !self.dryrun {
            mount(
                Some(source),
                &target,
                Some(filesystem.fs_type().to_mount_type()),
                MsFlags::MS_NOATIME,
                options,
            )?;
        } else {
            // TODO: add flags etc.
            println!(
                "mount {} {} -t {}",
                source.display(),
                target.display(),
                filesystem.fs_type().to_mount_type()
            );
        }
        self.targets.push(target);
        Ok(())
    }

    pub fn bind_mount(
        &mut self,
        source: PathBuf,
        target: PathBuf,
        options: Option<&str>,
    ) -> nix::Result<()> {
        debug!("Mounting {source:?} to {target:?}");
        if !self.dryrun {
            mount::<_, _, str, _>(
                Some(&source),
                &target,
                None,
                MsFlags::MS_BIND | MsFlags::MS_NOATIME, // Read-only flag has no effect for bind mounts
                options,
            )?;
        } else {
            // TODO: Add flags, etc.
            println!("mount --bind {} {}", source.display(), target.display());
        }
        self.targets.push(target);
        Ok(())
    }

    fn _umount(&mut self) -> anyhow::Result<()> {
        let mut result = Ok(());

        while let Some(target) = self.targets.pop() {
            debug!("Unmounting {}", target.display());

            if !self.dryrun {
                if let Err(e) = umount(&target) {
                    warn!("Unable to umount {}: {}", target.display(), e);
                    result = Err(anyhow!(
                        "Failed unmounting filesystem: {}, {}",
                        target.display(),
                        e
                    ));
                };
            } else {
                println!("umount {}", target.display());
            }
        }

        result
    }

    pub fn umount(mut self) -> anyhow::Result<()> {
        self._umount()
    }
}

impl<'a> Drop for MountStack<'a> {
    fn drop(&mut self) {
        self._umount().ok();
    }
}
