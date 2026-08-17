use shrimp::{Context, Script, ScriptOptions};
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("shrimp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> shrimp::Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut options = ScriptOptions::default();
    let mut check = false;
    while let Some(argument) = args.peek() {
        if argument == "--dry-run" {
            options.dry_run = true;
            args.next();
        } else if argument == "--trace" {
            options.trace = true;
            args.next();
        } else if argument == "--check" {
            check = true;
            args.next();
        } else {
            break;
        }
    }
    let Some(path) = args.next() else {
        eprintln!(
            "Usage: shrimp [--check] [--dry-run] [--trace] <workflow.shrimp> [NAME=VALUE ...]\n\nRun a portable Shrimp workflow. Extra NAME=VALUE arguments become variables."
        );
        return Err(shrimp::Error::message("missing script path"));
    };
    if path == "--help" || path == "-h" {
        println!(
            "Usage: shrimp [--check] [--dry-run] [--trace] <workflow.shrimp> [NAME=VALUE ...]"
        );
        return Ok(());
    }
    let path = PathBuf::from(path);
    let cwd = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut context = Context::new(
        std::fs::canonicalize(cwd)
            .map_err(|e| shrimp::Error::message(format!("resolve script directory: {e}")))?,
    );
    for argument in args {
        let argument = argument.to_string_lossy();
        let (name, value) = argument.split_once('=').ok_or_else(|| {
            shrimp::Error::message(format!("expected NAME=VALUE, got `{argument}`"))
        })?;
        context = context.with_env(name, value);
    }
    let script = Script::from_file(&path)?;
    if check {
        eprintln!("shrimp: syntax OK");
        return Ok(());
    }
    let report = script.run_with_options(&context, options)?;
    eprintln!(
        "shrimp: completed ({} commands, {} file changes)",
        report.commands_run, report.files_changed
    );
    Ok(())
}
