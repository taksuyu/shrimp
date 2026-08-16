//! Portable building blocks for scripts that need shell-like process control.
//!
//! Shrimp deliberately does not invoke a shell. Commands are represented as data,
//! arguments stay arguments, and composition happens through [`Task`] and [`Pipeline`].

mod command;
mod error;
mod fs;
mod script;
mod task;

pub use command::{Cmd, CommandOutput, Pipeline};
pub use error::{Error, Result};
pub use script::{Script, ScriptOptions, ScriptReport};
pub use task::{Context, Task};

/// Filesystem operations lifted into reusable [`Task`] values.
pub mod files {
    pub use crate::fs::{
        copy, create_dir_all, read, read_to_string, remove_file, write, write_atomic,
    };
}

/// Starts a command without going through a platform shell.
pub fn cmd(program: impl Into<std::ffi::OsString>) -> Cmd {
    Cmd::new(program)
}
