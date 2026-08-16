use crate::{Error, Result, Task};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

fn io<T>(operation: &'static str, path: &Path, result: std::io::Result<T>) -> Result<T> {
    result.map_err(|source| Error::io(operation, Some(path.to_owned()), source))
}

pub fn read(path: impl Into<PathBuf>) -> Task<Vec<u8>> {
    let path = path.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        io("read", &path, std::fs::read(&path))
    })
}
pub fn read_to_string(path: impl Into<PathBuf>) -> Task<String> {
    let path = path.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        io("read", &path, std::fs::read_to_string(&path))
    })
}
pub fn write(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Task<()> {
    let path = path.into();
    let contents = contents.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        io("write", &path, std::fs::write(&path, &contents))
    })
}
pub fn create_dir_all(path: impl Into<PathBuf>) -> Task<()> {
    let path = path.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        io("create directory", &path, std::fs::create_dir_all(&path))
    })
}
pub fn copy(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Task<u64> {
    let from = from.into();
    let to = to.into();
    Task::new(move |ctx| {
        let from = ctx.cwd().join(&from);
        let to = ctx.cwd().join(&to);
        io("copy", &to, std::fs::copy(from, &to))
    })
}
pub fn remove_file(path: impl Into<PathBuf>) -> Task<()> {
    let path = path.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        io("remove", &path, std::fs::remove_file(&path))
    })
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Writes beside the destination, then renames, preventing readers from seeing partial contents.
pub fn write_atomic(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Task<()> {
    let path = path.into();
    let contents = contents.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("shrimp-{}-{id}.tmp", std::process::id()));
        io(
            "write temporary file",
            &temp,
            std::fs::write(&temp, &contents),
        )?;
        if let Err(source) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::io("rename temporary file", Some(path), source));
        }
        Ok(())
    })
}
