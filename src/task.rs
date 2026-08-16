use crate::{Error, Result};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

/// Runtime-neutral inputs shared by tasks. It is cheap to clone and does not mutate
/// the calling process' current directory or environment.
#[derive(Clone, Debug)]
pub struct Context {
    cwd: PathBuf,
    env: HashMap<OsString, OsString>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: HashMap::new(),
        }
    }
}

impl Context {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            env: HashMap::new(),
        }
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn env(&self) -> &HashMap<OsString, OsString> {
        &self.env
    }
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }
}

/// A lazy, reusable computation, similar to a very small `Effect<R, E, A>`.
///
/// Tasks only run when [`Task::run`] is called. The `Send + Sync + 'static` bounds
/// allow future schedulers to execute the same task graph concurrently.
type Operation<T> = dyn Fn(&Context) -> Result<T> + Send + Sync;

pub struct Task<T> {
    operation: Arc<Operation<T>>,
}

impl<T> Clone for Task<T> {
    fn clone(&self) -> Self {
        Self {
            operation: Arc::clone(&self.operation),
        }
    }
}

impl<T: 'static> Task<T> {
    pub fn new(operation: impl Fn(&Context) -> Result<T> + Send + Sync + 'static) -> Self {
        Self {
            operation: Arc::new(operation),
        }
    }

    pub fn succeed(value: T) -> Self
    where
        T: Clone + Send + Sync,
    {
        Self::new(move |_| Ok(value.clone()))
    }

    pub fn run(&self, context: &Context) -> Result<T> {
        (self.operation)(context)
    }

    pub fn map<U: 'static>(self, f: impl Fn(T) -> U + Send + Sync + 'static) -> Task<U> {
        Task::new(move |ctx| self.run(ctx).map(&f))
    }

    pub fn and_then<U: 'static>(self, f: impl Fn(T) -> Task<U> + Send + Sync + 'static) -> Task<U> {
        Task::new(move |ctx| f(self.run(ctx)?).run(ctx))
    }

    pub fn tap(self, f: impl Fn(&T) + Send + Sync + 'static) -> Self {
        Task::new(move |ctx| {
            let value = self.run(ctx)?;
            f(&value);
            Ok(value)
        })
    }

    /// Retries failures up to `retries` times, sleeping between attempts.
    pub fn retry(self, retries: usize, delay: Duration) -> Self {
        Task::new(move |ctx| {
            let mut attempt = 0;
            loop {
                match self.run(ctx) {
                    Ok(value) => return Ok(value),
                    Err(error) if attempt == retries => return Err(error),
                    Err(_) => {
                        attempt += 1;
                        if !delay.is_zero() {
                            thread::sleep(delay);
                        }
                    }
                }
            }
        })
    }

    /// Runs on a worker thread and returns when the deadline expires. The worker
    /// cannot be forcibly cancelled; command-specific cancellation is a future API.
    pub fn timeout(self, limit: Duration) -> Self
    where
        T: Send,
    {
        Task::new(move |ctx| {
            let task = self.clone();
            let context = ctx.clone();
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            thread::spawn(move || {
                let _ = sender.send(task.run(&context));
            });
            receiver
                .recv_timeout(limit)
                .map_err(|_| Error::Timeout { limit })?
        })
    }
}
