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
    arguments: HashMap<String, String>,
}

impl Default for Context {
    /// Creates an empty context using the current working directory.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::default();
    /// assert!(context.env.is_empty());
    /// assert!(context.arguments.is_empty());
    /// ```
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: HashMap::new(),
            arguments: HashMap::new(),
        }
    }
}

impl Context {
    /// Creates a context with the specified working directory and empty environment and argument maps.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::new("/workspace");
    /// assert_eq!(context.cwd, std::path::PathBuf::from("/workspace"));
    /// assert!(context.env.is_empty());
    /// assert!(context.arguments.is_empty());
    /// ```
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            env: HashMap::new(),
            arguments: HashMap::new(),
        }
    }
    /// Provides the working directory associated with this context.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::default();
    /// assert_eq!(context.cwd(), std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    /// ```
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn env(&self) -> &HashMap<OsString, OsString> {
        &self.env
    }
    /// Adds an environment variable to the context.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::default().with_env("MODE", "test");
    ///
    /// assert_eq!(
    ///     context.env.get("MODE"),
    ///     Some(&std::ffi::OsString::from("test"))
    /// );
    /// ```
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
    /// Adds a workflow argument without adding it to child environments.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::default().with_argument("name", "value");
    /// assert_eq!(context.arguments.get("name"), Some(&"value".to_owned()));
    /// ```
    pub fn with_argument(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.arguments.insert(key.into(), value.into());
        self
    }
    /// Provides access to the workflow arguments stored in the context.
    ///
    /// # Examples
    ///
    /// ```text
    /// let context = Context::default();
    /// assert!(context.arguments().is_empty());
    /// ```
    pub(crate) fn arguments(&self) -> &HashMap<String, String> {
        &self.arguments
    }
    /// Replaces the working directory used by the context.
    ///
    /// # Examples
    ///
    /// ```
    /// use shrimp::Context;
    ///
    /// let context = Context::default().with_cwd("/tmp/workflow");
    /// assert_eq!(context.cwd(), std::path::Path::new("/tmp/workflow"));
    /// ```
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
            match receiver.recv_timeout(limit) {
                Ok(result) => result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout { limit }),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Error::message(
                    "task worker stopped without returning a result",
                )),
            }
        })
    }
}
