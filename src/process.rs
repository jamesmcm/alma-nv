use anyhow::anyhow;
use log::{debug, error};
use std::process::Command;
use std::str;

pub trait CommandExt {
    fn run(&mut self, dryrun: bool) -> anyhow::Result<()>;
    fn run_text_output(&mut self, dryrun: bool) -> anyhow::Result<String>;
}

impl CommandExt for Command {
    fn run(&mut self, dryrun: bool) -> anyhow::Result<()> {
        let command_string = format!(
            "{} {}",
            self.get_program().to_string_lossy(),
            self.get_args()
                .map(|x| x.to_string_lossy().to_string())
                .collect::<Vec<String>>()
                .join(" ")
        );
        debug!("Running command: {command_string}");

        if dryrun {
            println!("{command_string}");
            return Ok(());
        }

        let exit_status = self.spawn()?.wait()?;

        if !exit_status.success() {
            return Err(anyhow!("Bad exit code: {}", exit_status));
        }

        Ok(())
    }

    fn run_text_output(&mut self, dryrun: bool) -> anyhow::Result<String> {
        let command_string = format!(
            "{} {}",
            self.get_program().to_string_lossy(),
            self.get_args()
                .map(|x| x.to_string_lossy().to_string())
                .collect::<Vec<String>>()
                .join(" ")
        );
        debug!("Running command: {command_string}");

        if dryrun {
            println!("{command_string}");
            return Ok(String::from(""));
        }

        let output = self.output()?;

        if !output.status.success() {
            let error = str::from_utf8(&output.stderr).unwrap_or("[INVALID UTF8]");
            error!("{error}");
            return Err(anyhow!("Bad exit code: {}", output.status));
        }

        Ok(String::from(str::from_utf8(&output.stdout).map_err(
            |_| anyhow!("Process output is not valid UTF-8"),
        )?))
    }
}
