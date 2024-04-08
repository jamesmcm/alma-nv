use crate::{process::CommandExt, tool::Tool};
use anyhow::Context;
use log::info;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LoopDevice {
    path: PathBuf,
    losetup: Tool,
    dryrun: bool,
}

impl LoopDevice {
    pub fn create(file: &Path, dryrun: bool) -> anyhow::Result<Self> {
        let losetup = Tool::find("losetup", dryrun)?;
        let output = losetup
            .execute()
            .args(["--find", "-P", "--show"])
            .arg(file)
            .run_text_output(dryrun)
            .context("Error creating the image")?;

        let path = if dryrun {
            PathBuf::from("/dev/loop1337")
        } else {
            PathBuf::from(output.trim())
        };
        info!("Mounted {} to {}", file.display(), path.display());

        Ok(Self {
            path,
            losetup,
            dryrun,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        info!("Detaching loop device {}", self.path.display());
        self.losetup
            .execute()
            .arg("-d")
            .arg(&self.path)
            .run(self.dryrun)
            .ok();
    }
}
