use anyhow::anyhow;
use log::{debug, error};
use std::io::Write;
use std::process::{Command, Stdio};
use std::str;

pub trait CommandExt {
    fn run(&mut self, dryrun: bool) -> anyhow::Result<()>;
    fn run_text_output(&mut self, dryrun: bool) -> anyhow::Result<String>;
    /// Runs the command, writing `stdin` to its stdin. Used to pass sensitive
    /// data (e.g. a LUKS passphrase) to a `--key-file -` style command without
    /// exposing it on the command line.
    fn run_with_stdin(&mut self, stdin: &[u8], dryrun: bool) -> anyhow::Result<()>;
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

    fn run_with_stdin(&mut self, stdin: &[u8], dryrun: bool) -> anyhow::Result<()> {
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
            println!("{command_string} <stdin>");
            return Ok(());
        }

        let mut child = self
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn {command_string}: {e}"))?;

        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdin for {command_string}"))?
            .write_all(stdin)
            .map_err(|e| anyhow!("Failed to write to stdin of {command_string}: {e}"))?;

        let status = child
            .wait()
            .map_err(|e| anyhow!("Failed to wait on {command_string}: {e}"))?;
        if !status.success() {
            return Err(anyhow!("Bad exit code: {}", status));
        }
        Ok(())
    }
}
