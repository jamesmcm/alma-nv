mod args;
mod aur;
mod constants;
mod create;
mod initcpio;
mod install;
mod interactive;
mod presets;
mod process;
mod storage;
mod tool;

use anyhow::Result;
use args::Command;
use clap::Parser;
use log::LevelFilter;

fn main() -> Result<()> {
    let app = args::App::parse();

    let mut builder = pretty_env_logger::formatted_timed_builder();
    let log_level = if app.verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    builder.filter_level(log_level);
    builder.init();

    match app.cmd {
        Command::Create(command) => create::create(command),
        Command::Install(command) => install::install(command),
        Command::Chroot(command) => tool::chroot(command),
        Command::Qemu(command) => tool::qemu(command),
    }
}
