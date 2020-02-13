mod events;
mod handler;
mod keybindings;
mod ui_commands;

use std::sync::Arc;
use std::process::Stdio;

use rmpv::Value;
use nvim_rs::{create::tokio as create, UiAttachOptions};
use tokio::runtime::Runtime;
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use log::{info, error, trace};

pub use events::*;
pub use keybindings::*;
pub use ui_commands::UiCommand;
use handler::NeovimHandler;
use crate::error_handling::ResultPanicExplanation;
use crate::settings::{Settings, SETTINGS};
use crate::INITIAL_DIMENSIONS;


lazy_static! {
    pub static ref BRIDGE: Bridge = Bridge::new();
}

#[cfg(target_os = "windows")]
fn set_windows_creation_flags(cmd: &mut Command) {
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
}

fn create_nvim_command() -> Command {
    let mut cmd = Command::new("nvim");

    cmd.arg("--embed")
        .args(SETTINGS.lock().neovim_arguments.iter().skip(1))
        .stderr(Stdio::inherit());

    #[cfg(target_os = "windows")]
    set_windows_creation_flags(&mut cmd);

    cmd
}

async fn drain(receiver: &mut UnboundedReceiver<UiCommand>) -> Option<Vec<UiCommand>> {
    if let Some(ui_command) = receiver.recv().await {
        let mut results = vec![ui_command];
        while let Ok(ui_command) = receiver.try_recv() {
            results.push(ui_command);
        }
        Some(results)
    } else {
        None
    }
}

async fn start_process(mut receiver: UnboundedReceiver<UiCommand>) {
    let (width, height) = INITIAL_DIMENSIONS;
    let (mut nvim, io_handler, _) = create::new_child_cmd(&mut create_nvim_command(), NeovimHandler()).await
        .unwrap_or_explained_panic("Could not locate or start the neovim process");

    tokio::spawn(async move {
        info!("Close watcher started");
        match io_handler.await {
            Err(join_error) => eprintln!("Error joining IO loop: '{}'", join_error),
            Ok(Err(error)) => {
                if !error.is_channel_closed() {
                    error!("Error: '{}'", error);
                }
            },
            Ok(Ok(())) => {}
        };
        std::process::exit(0);
    });

    if let Ok(Value::Integer(correct_version)) = nvim.eval("has(\"nvim-0.4\")").await {
        if correct_version.as_i64() != Some(1) {
            error!("Neovide requires version 0.4 or higher");
            std::process::exit(0);
        }
    } else {
        error!("Neovide requires version 0.4 or higher");
        std::process::exit(0);
    };

    nvim.set_var("neovide", Value::Boolean(true)).await
        .unwrap_or_explained_panic("Could not communicate with neovim process");
    let mut options = UiAttachOptions::new();
    options.set_linegrid_external(true);
    options.set_rgb(true);
    nvim.ui_attach(width as i64, height as i64, &options).await
        .unwrap_or_explained_panic("Could not attach ui to neovim process");
    info!("Neovim process attached");

    let nvim = Arc::new(nvim);
    let input_nvim = nvim.clone();
    tokio::spawn(async move {
        info!("UiCommand processor started");
        while let Some(commands) = drain(&mut receiver).await {
            let (resize_list, other_commands): (Vec<UiCommand>, Vec<UiCommand>) = commands
                .into_iter()
                .partition(|command| command.is_resize());

            for command in resize_list
                .into_iter().last().into_iter()
                .chain(other_commands.into_iter()) {

                let input_nvim = input_nvim.clone();
                tokio::spawn(async move {
                    trace!("Executing UiCommand: {:?}", &command);
                    command.execute(&input_nvim).await;
                });
            }
        }
    });

    let mut settings = Settings::new();

    settings.read_initial_values(&nvim).await;
    settings.setup_changed_listeners(&nvim).await;

    SETTINGS.data = settings.data;

    nvim.set_option("lazyredraw", Value::Boolean(false)).await
        .ok();
}

pub struct Bridge {
    _runtime: Runtime, // Necessary to keep runtime running
    sender: UnboundedSender<UiCommand>
}

impl Bridge {
    pub fn new() -> Bridge {
        let runtime = Runtime::new().unwrap();
        let (sender, receiver) = unbounded_channel::<UiCommand>();

        runtime.spawn(async move {
            start_process(receiver).await;
        });

        Bridge { _runtime: runtime, sender }
    }

    pub fn queue_command(&self, command: UiCommand) {
        trace!("UiCommand queued: {:?}", &command);
        self.sender.send(command)
            .unwrap_or_explained_panic(
                "Could not send UI command from the window system to the neovim process.");
    }
}
