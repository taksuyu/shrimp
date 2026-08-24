use crate::{Context, Error, Result, Task};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::Read,
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
}

impl Cmd {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }
    /// Connects this command to another command in an ordered pipeline.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```
    
    /// let pipeline = Cmd::new("echo").pipe(Cmd::new("cat"));
    
    /// ```
    pub fn pipe(self, next: Cmd) -> Pipeline {
        Pipeline {
            commands: vec![self, next],
            stdin: None,
        }
    }
    /// Creates a pipeline containing this command as its first command.
    ///
    /// # Examples
    ///
    /// ```
    /// let pipeline = Cmd::new("echo").pipeline();
    /// ```
    pub fn pipeline(self) -> Pipeline {
        Pipeline {
            commands: vec![self],
            stdin: None,
        }
    }
    /// Wraps command execution in a task.
    ///
    /// # Examples
    ///
    /// ```
    /// let task = Cmd::new("echo").arg("hello").task();
    /// ```
    pub fn task(self) -> Task<CommandOutput> {
        Task::new(move |ctx| self.run(ctx))
    }
    /// Executes the command and returns its output.
    ///
    /// A non-successful exit status is returned as a command failure.
    ///
    /// # Examples
    ///
    /// ```
    /// let output = Cmd::new("echo")
    ///     .arg("hello")
    ///     .run(&Context::default())?;
    /// assert_eq!(output.stdout_string()?, "hello\n");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be executed or exits unsuccessfully.
    pub fn run(&self, context: &Context) -> Result<CommandOutput> {
        Pipeline {
            commands: vec![self.clone()],
            stdin: None,
        }
        .run(context)
    }
    /// Executes the command and returns its output regardless of its exit status.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let command = Cmd::new("echo").arg("hello");
    /// let output = command.run_unchecked(&Context::default())?;
    /// # let _ = output;
    /// # Ok::<(), Error>(())
    /// ```
    pub fn run_unchecked(&self, context: &Context) -> Result<CommandOutput> {
        Pipeline {
            commands: vec![self.clone()],
            stdin: None,
        }
        .run_unchecked(context)
    }

    fn command(&self, context: &Context, _isolated: bool) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir(self.cwd.as_deref().unwrap_or(context.cwd()));
        command
            .envs(context.env())
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        // Give each command its own process group so timeouts can also terminate
        // grandchildren (for example `sh -c "sleep 30"`) rather than leaking them.
        #[cfg(unix)]
        if _isolated {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        command
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", quote(&self.program))?;
        for arg in &self.args {
            write!(f, " {}", quote(arg))?;
        }
        Ok(())
    }
}

fn quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._/".contains(c))
    {
        value.into_owned()
    } else {
        format!("{:?}", value)
    }
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    commands: Vec<Cmd>,
    stdin: Option<Vec<u8>>,
}

type PipelineFailure = (String, ExitStatus, Vec<u8>);

impl Pipeline {
    /// Supplies input bytes to the first process in the pipeline without invoking a shell.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```
    
    /// let pipeline = Cmd::new("cat")
    
    ///     .pipe(Cmd::new("cat"))
    
    ///     .stdin("input");
    
    /// ```
    pub fn stdin(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }
    /// Appends a command to the pipeline.
    ///
    /// # Examples
    ///
    /// ```
    /// let pipeline = Cmd::new("echo").pipe(Cmd::new("cat"));
    /// ```
    pub fn pipe(mut self, next: Cmd) -> Self {
        self.commands.push(next);
        self
    }
    pub fn task(self) -> Task<CommandOutput> {
        Task::new(move |ctx| self.run(ctx))
    }

    pub fn run(&self, context: &Context) -> Result<CommandOutput> {
        let (output, failure) = self.execute(context)?;
        if let Some((command, status, stderr)) = failure {
            return Err(Error::CommandFailed {
                command,
                status,
                stderr,
            });
        }
        Ok(output)
    }

    /// Runs the pipeline and returns the final status even when it is non-zero.
    /// This is primarily useful for script conditions.
    pub fn run_unchecked(&self, context: &Context) -> Result<CommandOutput> {
        let (output, _) = self.execute(context)?;
        Ok(output)
    }

    /// Uses `pipefail` semantics: every command must exit successfully.
    pub fn is_success(&self, context: &Context) -> Result<bool> {
        let (_, failure) = self.execute(context)?;
        Ok(failure.is_none())
    }

    /// Executes the pipeline until all processes and output streams complete or the deadline expires.
    ///
    /// On expiry, terminates the pipeline's process tree and returns [`Error::Timeout`]. A non-successful
    /// command returns [`Error::CommandFailed`] using pipefail semantics.
    ///
    /// # Examples
    ///
    /// ```
    /// let pipeline = Cmd::new("printf").arg("done");
    /// let output = pipeline.run_timeout(&Context::default(), Duration::from_secs(1))?;
    /// assert_eq!(output.stdout_string()?, "done");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn run_timeout(&self, context: &Context, limit: Duration) -> Result<CommandOutput> {
        if self.commands.is_empty() {
            return Err(Error::EmptyPipeline);
        }
        let mut children: Vec<(String, std::process::Child, Option<ExitStatus>)> =
            Vec::with_capacity(self.commands.len());
        let mut previous_stdout = None;
        let mut final_stdout = None;
        let mut final_stderr = None;
        let mut stdin_writer = None;
        for (index, specification) in self.commands.iter().enumerate() {
            let last = index + 1 == self.commands.len();
            let mut command = specification.command(context, true);
            command.stdin(if index == 0 && self.stdin.is_some() {
                Stdio::piped()
            } else {
                previous_stdout
                    .take()
                    .map(Stdio::from)
                    .unwrap_or_else(Stdio::null)
            });
            command.stdout(Stdio::piped()).stderr(if last {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    for (_, child, _) in &mut children {
                        kill_process_tree(child);
                        let _ = child.wait();
                    }
                    return Err(Error::io("spawn command", None, error));
                }
            };
            if index == 0
                && let Some(input) = &self.stdin
            {
                stdin_writer = Some(spawn_stdin_writer(
                    child.stdin.take().expect("piped stdin"),
                    input.clone(),
                ));
            }
            if last {
                final_stdout = child.stdout.take();
                final_stderr = child.stderr.take();
            } else {
                previous_stdout = child.stdout.take();
            }
            children.push((specification.to_string(), child, None));
        }

        let stdout_reader = spawn_reader(final_stdout);
        let stderr_reader = spawn_reader(final_stderr);
        let mut stdout = None;
        let mut stderr = None;
        let mut stdin_complete = stdin_writer.is_none();
        let started = Instant::now();
        loop {
            let mut running = false;
            for (_, child, status) in &mut children {
                if status.is_none() {
                    *status = child
                        .try_wait()
                        .map_err(|e| Error::io("wait for command", None, e))?;
                    running |= status.is_none();
                }
            }
            poll_reader(&stdout_reader, &mut stdout)?;
            poll_reader(&stderr_reader, &mut stderr)?;
            if let Some(writer) = &stdin_writer
                && let Err(error) = poll_stdin_writer(writer, &mut stdin_complete)
            {
                for (_, child, _) in &mut children {
                    kill_process_tree(child);
                    let _ = child.wait();
                }
                return Err(error);
            }
            if !running && stdout.is_some() && stderr.is_some() && stdin_complete {
                break;
            }
            if started.elapsed() >= limit {
                for (_, child, _) in &mut children {
                    kill_process_tree(child);
                    let _ = child.wait();
                }
                return Err(Error::Timeout { limit });
            }
            thread::sleep(Duration::from_millis(5));
        }
        let stdout = stdout.expect("completed stdout reader");
        let stderr = stderr.expect("completed stderr reader");
        let final_status = children
            .last()
            .and_then(|(_, _, status)| *status)
            .expect("completed final process");
        if let Some((index, (name, _, Some(status)))) = children
            .iter()
            .enumerate()
            .find(|(_, (_, _, status))| status.is_some_and(|s| !s.success()))
        {
            let failure_stderr = if index + 1 == children.len() {
                stderr
            } else {
                Vec::new()
            };
            return Err(Error::CommandFailed {
                command: name.clone(),
                status: *status,
                stderr: failure_stderr,
            });
        }
        Ok(CommandOutput {
            status: final_status,
            stdout,
            stderr,
        })
    }

    /// Executes the pipeline and reports its output together with the first command failure, if any.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let (output, failure) = pipeline.execute(&context)?;
    /// assert!(failure.is_none());
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// Returns an error if the pipeline is empty or a command cannot be spawned or waited for.
    fn execute(&self, context: &Context) -> Result<(CommandOutput, Option<PipelineFailure>)> {
        if self.commands.is_empty() {
            return Err(Error::EmptyPipeline);
        }
        let mut children: Vec<(String, std::process::Child)> =
            Vec::with_capacity(self.commands.len());
        let mut previous_stdout = None;
        let mut stdin_writer = None;
        for (index, specification) in self.commands.iter().enumerate() {
            let last = index + 1 == self.commands.len();
            let mut command = specification.command(context, false);
            command.stdin(if index == 0 && self.stdin.is_some() {
                Stdio::piped()
            } else {
                previous_stdout
                    .take()
                    .map(Stdio::from)
                    .unwrap_or_else(Stdio::null)
            });
            // Intermediate stderr is inherited so a noisy child cannot fill an
            // unread pipe and deadlock the pipeline. The final stderr is captured.
            command.stdout(Stdio::piped()).stderr(if last {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    for (_, child) in &mut children {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return Err(Error::io("spawn command", None, error));
                }
            };
            if index == 0
                && let Some(input) = &self.stdin
            {
                stdin_writer = Some(spawn_stdin_writer(
                    child.stdin.take().expect("piped stdin"),
                    input.clone(),
                ));
            }
            if !last {
                previous_stdout = child.stdout.take();
            }
            children.push((specification.to_string(), child));
        }

        let (last_name, last_child) = children.pop().expect("pipeline is nonempty");
        let output = last_child
            .wait_with_output()
            .map_err(|e| Error::io("wait for command", None, e))?;
        let mut failure =
            (!output.status.success()).then(|| (last_name, output.status, output.stderr.clone()));
        for (name, mut child) in children {
            let status = child
                .wait()
                .map_err(|e| Error::io("wait for command", None, e))?;
            if failure.is_none() && !status.success() {
                failure = Some((name, status, Vec::new()));
            }
        }
        if let Some(writer) = stdin_writer {
            finish_stdin_writer(writer)?;
        }
        if let Some((command, status, stderr)) = failure {
            return Ok((
                CommandOutput {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                },
                Some((command, status, stderr)),
            ));
        }
        Ok((
            CommandOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            },
            None,
        ))
    }
}

/// Reads all available bytes from an optional reader.
///
/// Returns an empty vector when no reader is provided.
///
/// # Examples
///
/// ```
/// use std::io::Cursor;
///
/// let output = read_all(Some(Cursor::new(b"hello"))).unwrap();
/// assert_eq!(output, b"hello");
/// ```
fn read_all<R: Read>(reader: Option<R>) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(mut reader) = reader {
        reader
            .read_to_end(&mut output)
            .map_err(|e| Error::io("read command output", None, e))?;
    }
    Ok(output)
}

/// Writes command input on a background thread and reports the result through a channel.
///
/// Broken-pipe errors are treated as successful completion because the command may
/// exit before consuming all input.
///
/// # Examples
///
/// ```
/// use std::process::{Command, Stdio};
///
/// let mut child = Command::new("cat")
///     .stdin(Stdio::piped())
///     .spawn()
///     .unwrap();
/// let stdin = child.stdin.take().unwrap();
/// let result = spawn_stdin_writer(stdin, b"input".to_vec())
///     .recv()
///     .unwrap();
///
/// assert!(result.is_ok());
/// child.wait().unwrap();
/// ```
fn spawn_stdin_writer(mut stdin: std::process::ChildStdin, input: Vec<u8>) -> Receiver<Result<()>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        use std::io::Write as _;
        let result = stdin.write_all(&input).or_else(|error| {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(error)
            }
        });
        let _ = sender.send(result.map_err(|error| Error::io("write command stdin", None, error)));
    });
    receiver
}

/// Checks whether the asynchronous command stdin writer has completed and reports any failure.
///
/// # Examples
///
/// ```
/// use std::sync::mpsc::channel;
///
/// let (sender, receiver) = channel();
/// sender.send(Ok::<(), Error>(())).unwrap();
/// let mut complete = false;
///
/// poll_stdin_writer(&receiver, &mut complete).unwrap();
/// assert!(complete);
/// ```
fn poll_stdin_writer(receiver: &Receiver<Result<()>>, complete: &mut bool) -> Result<()> {
    if *complete {
        return Ok(());
    }
    match receiver.try_recv() {
        Ok(result) => {
            result?;
            *complete = true;
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            return Err(Error::message("command stdin writer stopped unexpectedly"));
        }
    }
    Ok(())
}

/// Waits for the stdin writer to finish and returns its result.
///
/// # Examples
///
/// ```
/// let (sender, receiver) = std::sync::mpsc::channel();
/// sender.send(Ok(())).unwrap();
///
/// assert!(finish_stdin_writer(receiver).is_ok());
/// ```
fn finish_stdin_writer(receiver: Receiver<Result<()>>) -> Result<()> {
    receiver
        .recv()
        .map_err(|_| Error::message("command stdin writer stopped unexpectedly"))?
}

/// Reads all data from an optional reader on a background thread.
///
/// # Examples
///
/// ```
/// let output = spawn_reader(Some(std::io::Cursor::new(b"hello".to_vec())))
///     .recv()
///     .unwrap()
///     .unwrap();
/// assert_eq!(output, b"hello");
/// ```
fn spawn_reader<R: Read + Send + 'static>(reader: Option<R>) -> Receiver<Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(read_all(reader));
    });
    receiver
}

fn poll_reader(receiver: &Receiver<Result<Vec<u8>>>, output: &mut Option<Vec<u8>>) -> Result<()> {
    if output.is_some() {
        return Ok(());
    }
    match receiver.try_recv() {
        Ok(result) => *output = Some(result?),
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            return Err(Error::message("command output reader stopped unexpectedly"));
        }
    }
    Ok(())
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        if libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) == -1 {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn stdout_string(&self) -> Result<String> {
        String::from_utf8(self.stdout.clone())
            .map_err(|e| Error::message(format!("command output was not UTF-8: {e}")))
    }
}
