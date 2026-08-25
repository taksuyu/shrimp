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
/// Writes byte contents to a path relative to the task context, creating missing parent directories.
///
/// # Examples
///
/// ```text
/// let task = write("output/data.txt", b"example".to_vec());
/// ```
pub fn write(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Task<()> {
    let path = path.into();
    let contents = contents.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        create_parent(&path)?;
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
/// Creates a task that copies a file to a context-relative destination, creating missing parent directories.
///
/// # Examples
///
/// ```text
/// let task = copy("source.txt", "backup/source.txt");
/// ```
///
/// # Arguments
///
/// * `from` - The context-relative source file path.
/// * `to` - The context-relative destination file path.
///
/// # Returns
///
/// The number of bytes copied.
///
/// # Errors
///
/// The task returns an error if the source cannot be read, the destination cannot be created or written, or a parent directory cannot be created.
pub fn copy(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Task<u64> {
    let from = from.into();
    let to = to.into();
    Task::new(move |ctx| {
        let from = ctx.cwd().join(&from);
        let to = ctx.cwd().join(&to);
        create_parent(&to)?;
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

/// Writes contents to a context-relative path by atomically replacing the destination.
///
/// Parent directories are created as needed, and readers see either the previous
/// contents or the complete new contents.
///
/// # Examples
///
/// ```text
/// let task = write_atomic("output/data.txt", b"complete contents".to_vec());
/// ```
pub fn write_atomic(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Task<()> {
    let path = path.into();
    let contents = contents.into();
    Task::new(move |ctx| {
        let path = ctx.cwd().join(&path);
        create_parent(&path)?;
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("shrimp-{}-{id}.tmp", std::process::id()));
        io(
            "write temporary file",
            &temp,
            std::fs::write(&temp, &contents),
        )?;
        if let Err(source) = atomic_replace(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::io("rename temporary file", Some(path), source));
        }
        Ok(())
    })
}

/// Creates all missing parent directories for a path.
///
/// # Examples
///
/// ```text
/// use std::fs;
/// use std::path::PathBuf;
///
/// let directory = std::env::temp_dir().join(format!(
///     "create-parent-example-{}",
///     std::process::id()
/// ));
/// let path = directory.join("nested").join("file.txt");
///
/// create_parent(&path)?;
/// assert!(path.parent().unwrap().is_dir());
///
/// fs::remove_dir_all(directory)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub(crate) fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        io(
            "create parent directories",
            parent,
            std::fs::create_dir_all(parent),
        )?;
    }
    Ok(())
}

/// Replaces the destination path with the file at the source path.
///
/// # Examples
///
/// ```text
/// # use std::path::Path;
/// # let source = Path::new("temporary-file");
/// # let destination = Path::new("destination-file");
/// # // `atomic_replace` replaces `destination` with `source`.
/// # let _ = (source, destination);
/// ```
///
/// # Parameters
///
/// * `from` — Path to the file being moved.
/// * `to` — Destination path to replace.
///
/// # Errors
///
/// Returns the underlying I/O error if the replacement fails.
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let succeeded = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
