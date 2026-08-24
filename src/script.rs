//! Parser and interpreter for Shrimp's portable workflow language.

use crate::{CommandOutput, Context, Error, Pipeline, Result, cmd, files};
use glob::glob;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

/// The intentionally small set of values understood by workflow code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Boolean(bool),
    Integer(i64),
    List(Vec<Value>),
    Record(HashMap<String, Value>),
    Missing,
}

impl Value {
    /// Converts a scalar value to its string representation.
    ///
    /// # Examples
    ///
    /// ```
    /// let value = Value::Integer(42);
    /// assert_eq!(value.scalar()?, "42");
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// Lists, records, and missing values cannot be converted to scalar text.
    fn scalar(&self) -> Result<String> {
        match self {
            Self::String(value) => Ok(value.clone()),
            Self::Boolean(value) => Ok(value.to_string()),
            Self::Integer(value) => Ok(value.to_string()),
            Self::Missing => Err(Error::message("missing value cannot be interpolated")),
            Self::List(_) => Err(Error::message(
                "list cannot be interpolated; index or iterate it",
            )),
            Self::Record(_) => Err(Error::message(
                "record cannot be interpolated; select a field",
            )),
        }
    }
}

const MAX_FUNCTION_CALL_DEPTH: usize = 64;

#[derive(Clone, Debug)]
pub struct Script {
    statements: Vec<Statement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptReport {
    pub commands_run: usize,
    pub files_changed: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptOptions {
    /// Print operations without executing commands or changing files.
    pub dry_run: bool,
    /// Print each operation immediately before it executes.
    pub trace: bool,
}

#[derive(Clone, Debug)]
struct Statement {
    line: usize,
    kind: StatementKind,
}

#[derive(Clone)]
struct FunctionDefinition {
    parameters: Vec<String>,
    body: Vec<Statement>,
    source_dir: PathBuf,
}

#[derive(Default)]
struct IncludeRegistry {
    loaded: HashMap<PathBuf, IncludeExports>,
    active: HashMap<PathBuf, thread::ThreadId>,
    waiting: HashMap<thread::ThreadId, PathBuf>,
}

#[derive(Clone, Default)]
struct IncludeExports {
    variables: HashMap<String, Value>,
    functions: HashMap<String, FunctionDefinition>,
    secrets: HashSet<String>,
}

/// Determines whether waiting on an include owner would create a cycle.
///
/// # Parameters
///
/// * `registry` — Tracks active includes and threads waiting for include ownership.
/// * `current` — The thread whose wait cycle is being checked.
/// * `owner` — The thread currently holding or awaiting include ownership.
/// * `include_chain` — Include paths active in the current thread.
///
/// # Returns
///
/// `true` if the wait would create a direct or indirect include cycle, `false` otherwise.
///
/// # Examples
///
/// ```rust,ignore
/// let cycle = include_wait_would_cycle(
///     &registry,
///     std::thread::current().id(),
///     owner,
///     &include_chain,
/// );
/// assert!(!cycle);
/// ```
fn include_wait_would_cycle(
    registry: &IncludeRegistry,
    current: thread::ThreadId,
    mut owner: thread::ThreadId,
    include_chain: &[PathBuf],
) -> bool {
    if registry
        .active
        .iter()
        .any(|(path, active_owner)| *active_owner == owner && include_chain.contains(path))
    {
        return true;
    }
    let mut visited = HashSet::new();
    while visited.insert(owner) {
        let Some(waited_path) = registry.waiting.get(&owner) else {
            return false;
        };
        let Some(next_owner) = registry.active.get(waited_path).copied() else {
            return false;
        };
        if next_owner == current {
            return true;
        }
        owner = next_owner;
    }
    false
}

#[derive(Clone, Debug)]
enum StatementKind {
    Input {
        name: String,
        environment: bool,
        secret: bool,
    },
    Let {
        name: String,
        value: String,
        secret: bool,
    },
    Capture {
        name: String,
        command: String,
    },
    Run(String),
    Retry {
        attempts: usize,
        command: String,
    },
    Timeout {
        duration: Duration,
        command: String,
    },
    Cd(String),
    WithCwd {
        path: String,
        body: Vec<Statement>,
    },
    Mkdir(String),
    Write {
        path: String,
        value: String,
        append: bool,
    },
    Copy {
        from: String,
        to: String,
    },
    Remove(String),
    RemoveTree(String),
    Record {
        name: String,
        value: String,
        fields: Vec<String>,
    },
    Print(String),
    If {
        command: String,
        yes: Vec<Statement>,
        no: Vec<Statement>,
    },
    Match {
        value: String,
        cases: Vec<(String, Vec<Statement>)>,
        fallback: Vec<Statement>,
    },
    For {
        name: String,
        values: Values,
        body: Vec<Statement>,
    },
    Parallel(Vec<Statement>),
    ParallelFor {
        name: String,
        values: Values,
        limit: usize,
        body: Vec<Statement>,
    },
    Function {
        name: String,
        parameters: Vec<String>,
        body: Vec<Statement>,
    },
    Call {
        target: Option<String>,
        name: String,
        arguments: String,
    },
    Value(String),
    Temp {
        name: String,
        directory: bool,
    },
    Metadata {
        name: String,
        path: String,
        modified: bool,
    },
    Include(String),
}

#[derive(Clone, Debug)]
enum Values {
    Glob(String),
    Lines(String),
    Words(String),
    Variable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    End,
    Else,
    Case,
}

impl Script {
    pub fn parse(source: &str) -> Result<Self> {
        let lines = logical_lines(source)?;
        let mut position = 0;
        let (statements, ending) = parse_block(&lines, &mut position, false)?;
        if ending.is_some() {
            return Err(script_error(
                lines[position.saturating_sub(1)].0,
                "unexpected block delimiter",
            ));
        }
        Ok(Self { statements })
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path)
            .map_err(|e| Error::io("read script", Some(path.into()), e))?;
        Self::parse(&source)
    }

    pub fn run(&self, context: &Context) -> Result<ScriptReport> {
        self.run_with_options(context, ScriptOptions::default())
    }

    /// Executes the script using the supplied context and execution options.
    ///
    /// Temporary resources created during execution are cleaned up before this method
    /// returns, including when execution fails.
    ///
    /// # Returns
    ///
    /// A report containing the script's execution results, or an error if execution
    /// fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let script = Script::parse("print \"Hello\"")?;
    /// let context = Context::default();
    /// let report = script.run_with_options(&context, ScriptOptions::default())?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn run_with_options(
        &self,
        context: &Context,
        options: ScriptOptions,
    ) -> Result<ScriptReport> {
        let mut runtime = Runtime {
            context: context.clone(),
            variables: HashMap::new(),
            secrets: HashSet::new(),
            functions: HashMap::new(),
            call_depth: 0,
            parallel_depth: 0,
            options,
            report: ScriptReport::default(),
            source_dirs: vec![context.cwd().to_owned()],
            includes: Arc::new((Mutex::new(IncludeRegistry::default()), Condvar::new())),
            include_chain: Vec::new(),
            last_value: Value::Missing,
            last_value_secret: false,
            assigned_variables: Vec::new(),
            temporary_paths: Arc::new(Mutex::new(Vec::new())),
        };
        let result = runtime
            .execute(&self.statements)
            .map(|()| runtime.report.clone());
        runtime.cleanup_temporaries();
        result
    }
}

#[derive(Clone)]
struct Runtime {
    context: Context,
    variables: HashMap<String, Value>,
    secrets: HashSet<String>,
    functions: HashMap<String, FunctionDefinition>,
    call_depth: usize,
    parallel_depth: usize,
    options: ScriptOptions,
    report: ScriptReport,
    source_dirs: Vec<PathBuf>,
    includes: Arc<(Mutex<IncludeRegistry>, Condvar)>,
    // Branch-local. Parallel clones inherit their logical ancestry so include
    // re-entry is detected even when the owning thread is blocked in `join`.
    include_chain: Vec<PathBuf>,
    last_value: Value,
    last_value_secret: bool,
    assigned_variables: Vec<String>,
    temporary_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl Runtime {
    /// Binds a value to a variable and updates whether the variable is marked as secret.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// runtime.bind("token".into(), Value::String("secret".into()), true);
    /// ```
    fn bind(&mut self, name: String, value: Value, secret: bool) {
        self.assigned_variables.push(name.clone());
        self.variables.insert(name.clone(), value);
        if secret {
            self.secrets.insert(name);
        } else {
            self.secrets.remove(&name);
        }
    }

    /// Determines whether a source expression references a configured secret.
    ///
    /// Secret references are detected both as direct variable roots and within interpolations,
    /// while respecting quoting and escaping rules.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert!(runtime.source_references_secret("${API_TOKEN}")?);
    /// assert!(!runtime.source_references_secret("'${API_TOKEN}'")?);
    /// # Ok::<(), Error>(())
    /// ```
    fn source_references_secret(&self, source: &str) -> Result<bool> {
        let source = source.trim();
        if !source.starts_with(['\'', '"']) && self.secrets.contains(secret_root(source)?) {
            return Ok(true);
        }
        let mut quote = None;
        let mut escaped = false;
        let chars: Vec<_> = source.char_indices().collect();
        let mut position = 0;
        while position < chars.len() {
            let (index, character) = chars[position];
            if escaped {
                escaped = false;
                position += 1;
                continue;
            }
            if character == '\\' && quote != Some('\'') {
                escaped = true;
            } else if character == '\'' || character == '"' {
                if quote == Some(character) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(character);
                }
            } else if character == '$' && quote != Some('\'') && source[index..].starts_with("${") {
                let rest = &source[index + 2..];
                let end = scan_first_delimiter(rest, &["}"])?
                    .map(|delimiter| delimiter.index)
                    .ok_or_else(|| Error::message("unclosed variable interpolation"))?;
                if self.secrets.contains(secret_root(&rest[..end])?) {
                    return Ok(true);
                }
            }
            position += 1;
        }
        Ok(false)
    }

    /// Executes statements in order and associates execution errors with their source lines.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn example(runtime: &mut Runtime) -> Result<()> {
    /// runtime.execute(&[])?;
    /// # Ok(())
    /// # }
    /// ```
    fn execute(&mut self, statements: &[Statement]) -> Result<()> {
        for statement in statements {
            self.execute_one(statement)
                .map_err(|error| attach_line(statement.line, error))?;
        }
        Ok(())
    }

    /// Executes one workflow statement and updates the runtime state.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn example(runtime: &mut Runtime, statement: &Statement) -> Result<()> {
    /// runtime.execute_one(statement)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The statement may update variables, execute commands, modify files, change
    /// the working directory, or invoke other workflow constructs. Dry-run mode
    /// records applicable operations without performing their external effects.
    fn execute_one(&mut self, statement: &Statement) -> Result<()> {
        self.last_value = Value::Missing;
        self.last_value_secret = false;
        match &statement.kind {
            StatementKind::Input {
                name,
                environment,
                secret,
            } => {
                let value = if *environment {
                    self.context
                        .env()
                        .get(std::ffi::OsStr::new(name))
                        .map(|value| value.to_string_lossy().into_owned())
                        .ok_or_else(|| {
                            Error::message(format!(
                                "required environment variable `{name}` is not set"
                            ))
                        })?
                } else {
                    self.context.arguments().get(name).cloned().ok_or_else(|| {
                        Error::message(format!(
                            "required workflow argument `{name}` was not provided"
                        ))
                    })?
                };
                self.bind(name.clone(), Value::String(value), *secret);
                self.last_value = self.variables[name].clone();
                self.last_value_secret = *secret;
                self.trace(&format!(
                    "use {} {name}",
                    if *environment { "env" } else { "arg" }
                ));
            }
            StatementKind::Let {
                name,
                value,
                secret,
            } => {
                let inherited_secret = self.source_references_secret(value)?;
                let value = self.eval_value(value)?;
                self.bind(name.clone(), value, *secret || inherited_secret);
                self.last_value = self.variables[name].clone();
                self.last_value_secret = *secret || inherited_secret;
            }
            StatementKind::Capture { name, command } => {
                self.trace_command("capture", command)?;
                if self.options.dry_run {
                    self.bind(name.clone(), Value::String(String::new()), false);
                } else {
                    let invocation = self.invocation(command)?;
                    if invocation.redirect.is_some() {
                        return Err(Error::message(
                            "capture cannot be combined with redirection",
                        ));
                    }
                    let output = invocation.run(&self.context)?;
                    self.report.commands_run += 1;
                    self.bind(
                        name.clone(),
                        Value::String(
                            output
                                .stdout_string()?
                                .trim_end_matches(['\r', '\n'])
                                .to_owned(),
                        ),
                        false,
                    );
                }
                self.last_value = self.variables[name].clone();
            }
            StatementKind::Run(command) => {
                self.trace_command("run", command)?;
                if !self.options.dry_run {
                    let invocation = self.invocation(command)?;
                    let output = invocation.pipeline.run(&self.context)?;
                    self.report.commands_run += 1;
                    invocation.finish(output, &self.context)?;
                    self.report.files_changed += usize::from(invocation.changes_file());
                }
            }
            StatementKind::Retry { attempts, command } => {
                if *attempts == 0 {
                    return Err(Error::message("retry count must be greater than zero"));
                }
                self.trace_command(&format!("retry {attempts}"), command)?;
                if !self.options.dry_run {
                    let invocation = self.invocation(command)?;
                    let mut last = None;
                    for attempt in 0..*attempts {
                        self.report.commands_run += 1;
                        match invocation.pipeline.run(&self.context) {
                            Ok(output) => {
                                invocation.finish(output, &self.context)?;
                                self.report.files_changed += usize::from(invocation.changes_file());
                                return Ok(());
                            }
                            Err(error) => {
                                last = Some(error);
                                if attempt + 1 < *attempts {
                                    thread::sleep(Duration::from_millis(100));
                                }
                            }
                        }
                    }
                    return Err(last.expect("at least one retry attempt"));
                }
            }
            StatementKind::Timeout { duration, command } => {
                self.trace_command(&format!("timeout {}ms", duration.as_millis()), command)?;
                if !self.options.dry_run {
                    let invocation = self.invocation(command)?;
                    let output = invocation.pipeline.run_timeout(&self.context, *duration)?;
                    self.report.commands_run += 1;
                    invocation.finish(output, &self.context)?;
                    self.report.files_changed += usize::from(invocation.changes_file());
                }
            }
            StatementKind::Cd(path) => {
                let path = resolve(self.context.cwd(), &self.expand_single(path)?);
                self.trace(&format!("cd {}", path.display()));
                self.context = self.context.clone().with_cwd(path);
            }
            StatementKind::WithCwd { path, body } => {
                let path = resolve(self.context.cwd(), &self.expand_single(path)?);
                self.trace(&format!("with cwd {}", path.display()));
                let saved = self.context.clone();
                self.context = self.context.clone().with_cwd(path);
                let result = self.execute(body);
                self.context = saved;
                result?;
            }
            StatementKind::Mkdir(path) => {
                let path = self.expand_single(path)?;
                self.trace(&format!("mkdir {path}"));
                if !self.options.dry_run {
                    files::create_dir_all(path).run(&self.context)?;
                    self.report.files_changed += 1;
                }
            }
            StatementKind::Write {
                path,
                value,
                append,
            } => {
                let path = resolve(self.context.cwd(), &self.expand_single(path)?);
                let value = self.expand_single(value)?;
                self.trace(&format!(
                    "{} {}",
                    if *append { "append" } else { "write" },
                    path.display()
                ));
                if !self.options.dry_run {
                    files::create_parent(&path)?;
                    if *append {
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .map_err(|e| Error::io("append", Some(path.clone()), e))?;
                        file.write_all(value.as_bytes())
                            .map_err(|e| Error::io("append", Some(path), e))?;
                    } else {
                        files::write_atomic(path, value).run(&self.context)?;
                    }
                    self.report.files_changed += 1;
                }
            }
            StatementKind::Copy { from, to } => {
                let from = self.expand_single(from)?;
                let to = self.expand_single(to)?;
                self.trace(&format!("copy {from} -> {to}"));
                if !self.options.dry_run {
                    files::copy(from, to).run(&self.context)?;
                    self.report.files_changed += 1;
                }
            }
            StatementKind::Remove(path) => {
                let path = self.expand_single(path)?;
                self.trace(&format!("remove {path}"));
                if !self.options.dry_run {
                    files::remove_file(path).run(&self.context)?;
                    self.report.files_changed += 1;
                }
            }
            StatementKind::RemoveTree(path) => {
                let path = self.expand_single(path)?;
                if path.is_empty() {
                    return Err(Error::message("remove path must not be empty"));
                }
                let path = resolve(self.context.cwd(), &path);
                self.trace(&format!("remove --recursive --force {}", path.display()));
                if !self.options.dry_run {
                    let metadata = match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                        Err(error) => {
                            return Err(Error::io(
                                "inspect for recursive removal",
                                Some(path),
                                error,
                            ));
                        }
                    };
                    let result = if metadata.file_type().is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    match result {
                        Ok(()) => self.report.files_changed += 1,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(Error::io("remove recursively", Some(path), error));
                        }
                    }
                }
            }
            StatementKind::Record {
                name,
                value,
                fields,
            } => {
                let secret = self.source_references_secret(value)?;
                let value = self.expand_single(value)?;
                let parts: Vec<_> = value.split('\t').collect();
                if parts.len() != fields.len() {
                    return Err(Error::message(format!(
                        "record `{name}` expects {} tab-separated fields, got {}",
                        fields.len(),
                        parts.len()
                    )));
                }
                let record = fields
                    .iter()
                    .zip(parts)
                    .map(|(field, value)| (field.clone(), Value::String(value.into())))
                    .collect();
                let value = Value::Record(record);
                self.bind(name.clone(), value.clone(), secret);
                self.last_value = value;
                self.last_value_secret = secret;
            }
            StatementKind::Print(value) => {
                let value = self.expand_single(value)?;
                self.trace(&format!("print {}", self.redact(value.clone())));
                if !self.options.dry_run {
                    println!("{value}");
                }
            }
            StatementKind::If { command, yes, no } => {
                let expression = self.is_expression(command)?;
                if expression {
                    self.trace(&format!("if {command}"));
                } else {
                    self.trace_command("if", command)?;
                }
                let success = if expression {
                    self.condition(command)?
                } else if self.options.dry_run {
                    true
                } else {
                    self.report.commands_run += 1;
                    self.invocation(command)?
                        .pipeline
                        .is_success(&self.context)?
                };
                self.execute(if success { yes } else { no })?;
            }
            StatementKind::Match {
                value,
                cases,
                fallback,
            } => {
                let value = self.expand_single(value)?;
                let mut body = fallback;
                for (pattern, candidate) in cases {
                    if self.expand_single(pattern)? == value {
                        body = candidate;
                        break;
                    }
                }
                self.execute(body)?;
            }
            StatementKind::For { name, values, body } => {
                for value in self.values(values)? {
                    self.variables.insert(name.clone(), Value::String(value));
                    self.execute(body)?;
                }
                self.variables.remove(name);
            }
            StatementKind::Parallel(branches) => self.parallel(branches)?,
            StatementKind::ParallelFor {
                name,
                values,
                limit,
                body,
            } => self.parallel_for(name, values, *limit, body)?,
            StatementKind::Function {
                name,
                parameters,
                body,
            } => {
                self.functions.insert(
                    name.clone(),
                    FunctionDefinition {
                        parameters: parameters.clone(),
                        body: body.clone(),
                        source_dir: self
                            .source_dirs
                            .last()
                            .expect("runtime always has a source directory")
                            .clone(),
                    },
                );
            }
            StatementKind::Call {
                target,
                name,
                arguments,
            } => {
                let (value, secret) = self.call(name, arguments)?;
                if let Some(target) = target {
                    self.bind(target.clone(), value.clone(), secret);
                }
                self.last_value = value;
                self.last_value_secret = secret;
            }
            StatementKind::Include(path) => self.include(path)?,
            StatementKind::Value(source) => {
                self.last_value = self.eval_value(source)?;
                self.last_value_secret = self.source_references_secret(source)?;
            }
            StatementKind::Temp { name, directory } => self.create_temporary(name, *directory)?,
            StatementKind::Metadata {
                name,
                path,
                modified,
            } => self.metadata(name, path, *modified)?,
        }
        Ok(())
    }

    /// Expands a value source into a vector of strings.
    ///
    /// Word sources are split on whitespace, line sources are split into lines, glob
    /// sources are resolved relative to the current working directory, and variable
    /// sources must contain a list of scalar values.
    ///
    /// # Errors
    ///
    /// Returns an error if expansion fails, a glob pattern is invalid or not
    /// representable as UTF-8, or a variable does not contain a list of scalar
    /// values.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let values = runtime.values(&Values::Lines("first\nsecond".into()))?;
    /// assert_eq!(values, ["first", "second"]);
    /// ```
    fn values(&self, values: &Values) -> Result<Vec<String>> {
        match values {
            Values::Words(value) => Ok(self
                .expand_single(value)?
                .split_whitespace()
                .map(str::to_owned)
                .collect()),
            Values::Lines(value) => Ok(self
                .expand_single(value)?
                .lines()
                .map(str::to_owned)
                .collect()),
            Values::Glob(pattern) => {
                let pattern = resolve(self.context.cwd(), &self.expand_single(pattern)?);
                let pattern = pattern
                    .to_str()
                    .ok_or_else(|| Error::message("glob pattern is not UTF-8"))?;
                let mut paths = glob(pattern)
                    .map_err(|e| Error::message(format!("invalid glob: {e}")))?
                    .map(|entry| entry.map_err(|e| Error::message(format!("glob: {e}"))))
                    .collect::<Result<Vec<_>>>()?;
                paths.sort();
                Ok(paths
                    .into_iter()
                    .map(|path| {
                        path.strip_prefix(self.context.cwd())
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect())
            }
            Values::Variable(name) => match lookup(&self.variables, name)? {
                Value::List(values) => values.iter().map(Value::scalar).collect(),
                _ => Err(Error::message("for variable source must be a list")),
            },
        }
    }

    fn parallel(&mut self, branches: &[Statement]) -> Result<()> {
        self.trace(&format!("parallel {} branches", branches.len()));
        if self.options.dry_run || self.parallel_depth > 0 {
            let mut first_error = None;
            for branch in branches {
                let mut runtime = self.clone();
                runtime.report = ScriptReport::default();
                match runtime.execute(std::slice::from_ref(branch)) {
                    Ok(()) => {
                        self.report.commands_run += runtime.report.commands_run;
                        self.report.files_changed += runtime.report.files_changed;
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            return first_error.map_or(Ok(()), Err);
        }
        let handles: Vec<_> = branches
            .iter()
            .cloned()
            .map(|branch| {
                let mut runtime = self.clone();
                runtime.parallel_depth += 1;
                thread::spawn(move || {
                    runtime.report = ScriptReport::default();
                    runtime.execute(&[branch])?;
                    Ok::<_, Error>(runtime.report)
                })
            })
            .collect();
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(report)) => {
                    self.report.commands_run += report.commands_run;
                    self.report.files_changed += report.files_changed;
                }
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(Error::message("parallel branch panicked"));
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Executes a statement body for each value, subject to the specified concurrency limit.
    ///
    /// Values are processed in batches of at most `limit` items. Nested parallel execution
    /// runs sequentially, while dry-run execution preserves the same iteration order without
    /// spawning threads. Command and file-change counts from successful branches are aggregated.
    ///
    /// # Errors
    ///
    /// Returns an error if `limit` is zero, a branch fails, or a parallel branch panics.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn example(runtime: &mut Runtime, values: &Values, body: &[Statement]) -> Result<()> {
    /// runtime.parallel_for("item", values, 4, body)?;
    /// # Ok(())
    /// # }
    /// ```
    fn parallel_for(
        &mut self,
        name: &str,
        values: &Values,
        limit: usize,
        body: &[Statement],
    ) -> Result<()> {
        if limit == 0 {
            return Err(Error::message("parallel limit must be greater than zero"));
        }
        let values = self.values(values)?;
        self.trace(&format!(
            "parallel for {} items (limit {limit})",
            values.len()
        ));
        if self.parallel_depth > 0 {
            let mut first_error = None;
            for value in values {
                let mut runtime = self.clone();
                runtime.variables.insert(name.into(), Value::String(value));
                runtime.report = ScriptReport::default();
                match runtime.execute(body) {
                    Ok(()) => {
                        self.report.commands_run += runtime.report.commands_run;
                        self.report.files_changed += runtime.report.files_changed;
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            return first_error.map_or(Ok(()), Err);
        }
        for batch in values.chunks(limit) {
            let handles: Vec<_> = batch
                .iter()
                .map(|value| {
                    let mut runtime = self.clone();
                    runtime.parallel_depth += 1;
                    runtime
                        .variables
                        .insert(name.into(), Value::String(value.clone()));
                    runtime.report = ScriptReport::default();
                    let body = body.to_vec();
                    if self.options.dry_run {
                        None
                    } else {
                        Some(thread::spawn(move || {
                            runtime.execute(&body)?;
                            Ok::<_, Error>(runtime.report)
                        }))
                    }
                })
                .collect();
            if self.options.dry_run {
                for value in batch {
                    let mut runtime = self.clone();
                    runtime
                        .variables
                        .insert(name.into(), Value::String(value.clone()));
                    runtime.report = ScriptReport::default();
                    runtime.execute(body)?;
                }
                continue;
            }
            let mut first_error = None;
            for handle in handles.into_iter().flatten() {
                match handle.join() {
                    Ok(Ok(report)) => {
                        self.report.commands_run += report.commands_run;
                        self.report.files_changed += report.files_changed;
                    }
                    Ok(Err(error)) => {
                        first_error.get_or_insert(error);
                    }
                    Err(_) => {
                        first_error
                            .get_or_insert_with(|| Error::message("parallel branch panicked"));
                    }
                };
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Executes a named function with evaluated arguments and returns its final value and secrecy status.
    ///
    /// Function parameters are bound for the duration of the call, while the caller's variable and value
    /// state is restored afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if the function is undefined, receives an incorrect number of arguments, or
    /// exceeds the maximum call depth.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let (value, is_secret) = runtime.call("greet", "\"world\"")?;
    /// assert!(!is_secret);
    /// # Ok::<(), Error>(())
    /// ```
    fn call(&mut self, name: &str, arguments: &str) -> Result<(Value, bool)> {
        if self.call_depth >= MAX_FUNCTION_CALL_DEPTH {
            return Err(Error::message(format!(
                "function call depth exceeded the limit of {MAX_FUNCTION_CALL_DEPTH}"
            )));
        }
        let definition = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| Error::message(format!("undefined function `{name}`")))?;
        let arguments = argument_sources(arguments)?
            .into_iter()
            .map(|argument| {
                Ok((
                    self.eval_value(argument)?,
                    self.source_references_secret(argument)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if arguments.len() != definition.parameters.len() {
            return Err(Error::message(format!(
                "function `{name}` expects {} arguments, got {}",
                definition.parameters.len(),
                arguments.len()
            )));
        }
        let saved = self.variables.clone();
        let saved_secrets = self.secrets.clone();
        let saved_assignments = self.assigned_variables.len();
        for (parameter, (argument, secret)) in definition.parameters.into_iter().zip(arguments) {
            self.bind(parameter, argument, secret);
        }
        self.call_depth += 1;
        let saved_value = self.last_value.clone();
        let saved_value_secret = self.last_value_secret;
        self.source_dirs.push(definition.source_dir);
        let result = self
            .execute(&definition.body)
            .map(|()| (self.last_value.clone(), self.last_value_secret));
        self.source_dirs.pop();
        self.call_depth -= 1;
        self.variables = saved;
        self.secrets = saved_secrets;
        self.assigned_variables.truncate(saved_assignments);
        self.last_value = saved_value;
        self.last_value_secret = saved_value_secret;
        result
    }

    /// Creates a temporary file or directory, binds its path to a variable, and records it for cleanup.
    ///
    /// # Arguments
    ///
    /// * `name` - Variable that receives the temporary resource path.
    /// * `directory` - Whether to create a directory instead of a file.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the resource is created and bound, or an error if a resource name cannot be chosen or the resource cannot be created.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// runtime.create_temporary("tmp_path", false)?;
    /// ```
    fn create_temporary(&mut self, name: &str, directory: bool) -> Result<()> {
        let path = if self.options.dry_run {
            temporary_candidate()
                .map_err(|error| Error::io("choose temporary resource name", None, error))?
        } else {
            create_temporary_resource(directory, temporary_candidate).map_err(|(path, error)| {
                Error::io(
                    if directory {
                        "create temporary directory"
                    } else {
                        "create temporary file"
                    },
                    path,
                    error,
                )
            })?
        };
        self.trace(&format!(
            "{} {}",
            if directory { "temp_dir" } else { "temp_file" },
            path.display()
        ));
        if !self.options.dry_run {
            self.temporary_paths
                .lock()
                .expect("temporary path registry poisoned")
                .push(path.clone());
        }
        let value = Value::String(path.to_string_lossy().into_owned());
        self.bind(name.into(), value.clone(), false);
        self.last_value = value;
        Ok(())
    }

    /// Removes all registered temporary files and directories.
    ///
    /// Cleanup proceeds in reverse registration order, and removal errors are ignored.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// runtime.cleanup_temporaries();
    /// ```
    fn cleanup_temporaries(&self) {
        let mut paths = self
            .temporary_paths
            .lock()
            .expect("temporary path registry poisoned");
        for path in paths.drain(..).rev() {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }

    /// Stores a file's size or modification time in a runtime variable.
    ///
    /// In dry-run mode, stores `0` regardless of the file's metadata. The `modified`
    /// parameter selects modification time when `true` and file size when `false`.
    ///
    /// # Arguments
    ///
    /// * `name` - The variable name to bind.
    /// * `source` - The path of the file whose metadata is read.
    /// * `modified` - Whether to store modification time instead of file size.
    ///
    /// # Returns
    ///
    /// `Ok(())` after binding the metadata value.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(runtime: &mut Runtime) -> Result<()> {
    /// runtime.metadata("file_size", "file.txt", false)?;
    /// # Ok(())
    /// # }
    /// ```
    fn metadata(&mut self, name: &str, source: &str, modified: bool) -> Result<()> {
        let path = resolve(self.context.cwd(), &self.expand_single(source)?);
        self.trace(&format!(
            "{} {}",
            if modified {
                "modified_time"
            } else {
                "file_size"
            },
            path.display()
        ));
        if self.options.dry_run {
            let value = Value::Integer(0);
            self.bind(name.into(), value.clone(), false);
            self.last_value = value;
            return Ok(());
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|e| Error::io("read metadata", Some(path.clone()), e))?;
        let number = if modified {
            metadata
                .modified()
                .map_err(|e| Error::io("read modification time", Some(path.clone()), e))?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::message("modification time is before the Unix epoch"))?
                .as_secs()
                .try_into()
                .map_err(|_| Error::message("modification time does not fit in an integer"))?
        } else {
            metadata
                .len()
                .try_into()
                .map_err(|_| Error::message("file size does not fit in an integer"))?
        };
        let value = Value::Integer(number);
        self.bind(name.into(), value.clone(), false);
        self.last_value = value;
        Ok(())
    }

    /// Includes and executes a script from a path relative to the current source file.
    ///
    /// Successfully included scripts export their newly assigned variables, functions, and
    /// secret bindings to the current runtime. Previously loaded scripts are reused.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be expanded, resolved, read, parsed, or executed,
    /// or if including it would create an include cycle.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let mut runtime = Runtime::new();
    /// runtime.include("setup.shrimp")?;
    /// # Ok::<(), Error>(())
    /// ```
    fn include(&mut self, source: &str) -> Result<()> {
        let requested = self.expand_single(source)?;
        let base = self
            .source_dirs
            .last()
            .expect("runtime always has a source directory");
        let path = resolve(base, &requested);
        let path = std::fs::canonicalize(&path)
            .map_err(|error| Error::io("resolve include", Some(path), error))?;
        if self.include_chain.contains(&path) {
            return Err(Error::message(format!(
                "include cycle detected at {}",
                path.display()
            )));
        }
        let current_thread = thread::current().id();
        let includes = Arc::clone(&self.includes);
        let (lock, changed) = &*includes;
        loop {
            let mut registry = lock.lock().expect("include registry poisoned");
            if let Some(exports) = registry.loaded.get(&path).cloned() {
                drop(registry);
                for (name, value) in exports.variables {
                    let secret = exports.secrets.contains(&name);
                    self.bind(name, value, secret);
                }
                self.functions.extend(exports.functions);
                self.secrets.extend(exports.secrets);
                return Ok(());
            }
            if let Some(owner) = registry.active.get(&path).copied() {
                if owner == current_thread
                    || include_wait_would_cycle(
                        &registry,
                        current_thread,
                        owner,
                        &self.include_chain,
                    )
                {
                    return Err(Error::message(format!(
                        "include cycle detected at {}",
                        path.display()
                    )));
                }
                registry.waiting.insert(current_thread, path.clone());
                let mut registry = changed.wait(registry).expect("include registry poisoned");
                registry.waiting.remove(&current_thread);
                continue;
            }
            registry.active.insert(path.clone(), current_thread);
            break;
        }
        let assignments_before = self.assigned_variables.len();
        let functions_before = self.functions.clone();
        let secrets_before = self.secrets.clone();
        self.include_chain.push(path.clone());
        let result = (|| {
            let source = std::fs::read_to_string(&path)
                .map_err(|error| Error::io("read include", Some(path.clone()), error))?;
            let script = Script::parse(&source).map_err(|error| {
                Error::message(format!("in included file {}: {error}", path.display()))
            })?;
            self.source_dirs
                .push(path.parent().expect("included file has parent").to_owned());
            let result = self.execute(&script.statements).map_err(|error| {
                Error::message(format!("in included file {}: {error}", path.display()))
            });
            self.source_dirs.pop();
            result
        })();
        self.include_chain.pop();
        let mut registry = lock.lock().expect("include registry poisoned");
        registry.active.remove(&path);
        if result.is_ok() {
            let assigned: HashSet<_> = self.assigned_variables[assignments_before..]
                .iter()
                .cloned()
                .collect();
            let variables = assigned
                .iter()
                .filter_map(|name| {
                    self.variables
                        .get(name)
                        .map(|value| (name.clone(), value.clone()))
                })
                .collect();
            let functions = self
                .functions
                .iter()
                .filter(|(name, _)| !functions_before.contains_key(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let secrets = self.secrets.difference(&secrets_before).cloned().collect();
            registry.loaded.insert(
                path,
                IncludeExports {
                    variables,
                    functions,
                    secrets,
                },
            );
        }
        changed.notify_all();
        result
    }

    /// Expands `source` and requires it to produce exactly one value.
    ///
    /// # Errors
    ///
    /// Returns an error if expansion produces zero or multiple values.
    ///
    /// # Examples
    ///
    /// ```
    /// # let runtime = /* a configured runtime */ unimplemented!();
    /// let value = runtime.expand_single("hello")?;
    /// assert_eq!(value, "hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    fn expand_single(&self, source: &str) -> Result<String> {
        let values = words(source, &self.variables)?;
        if values.len() != 1 {
            return Err(Error::message(format!(
                "expected one value, found {}",
                values.len()
            )));
        }
        Ok(values.into_iter().next().expect("one value"))
    }

    /// Evaluates a source expression into a typed workflow value.
    ///
    /// Recognizes boolean and integer literals, word/line/glob expressions, variable
    /// references, and interpolated strings.
    ///
    /// # Examples
    ///
    /// ```
    /// let value = runtime.eval_value("42")?;
    /// assert_eq!(value, Value::Integer(42));
    /// ```
    fn eval_value(&self, source: &str) -> Result<Value> {
        let source = source.trim();
        if source == "true" {
            return Ok(Value::Boolean(true));
        }
        if source == "false" {
            return Ok(Value::Boolean(false));
        }
        if let Ok(value) = source.parse::<i64>() {
            return Ok(Value::Integer(value));
        }
        for (prefix, kind) in [("words ", 0), ("lines ", 1), ("glob ", 2)] {
            if let Some(rest) = source.strip_prefix(prefix) {
                let values = self.values(&match kind {
                    0 => Values::Words(rest.into()),
                    1 => Values::Lines(rest.into()),
                    _ => Values::Glob(rest.into()),
                })?;
                return Ok(Value::List(values.into_iter().map(Value::String).collect()));
            }
        }
        if source.starts_with("${") && source.ends_with('}') && source.matches("${").count() == 1 {
            let name = &source[2..source.len() - 1];
            if let Ok(value) = lookup(&self.variables, name) {
                return Ok(value.clone());
            }
        }
        Ok(Value::String(self.expand_single(source)?))
    }

    /// Evaluates a condition expression from its source text.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let enabled = runtime.condition("enabled && ready")?;
    /// assert!(enabled);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// Returns `true` when the condition evaluates to true, and `false` otherwise.
    fn condition(&self, source: &str) -> Result<bool> {
        let tokens = argument_sources(source)?;
        self.condition_tokens(&tokens)
    }

    /// Evaluates a tokenized boolean condition using logical operators, existence checks, and value comparisons.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let matches = runtime.condition_tokens(&["count", ">=", "1"])?;
    /// assert!(matches);
    /// ```
    ///
    /// Supports `or`, `and`, `not`, `exists`, equality comparisons, ordered integer
    /// comparisons, and standalone boolean operands. Returns an error for invalid
    /// expressions or ordered comparisons involving non-integer values.
    fn condition_tokens(&self, tokens: &[&str]) -> Result<bool> {
        if let Some(position) = tokens.iter().position(|v| *v == "or") {
            return Ok(self.condition_tokens(&tokens[..position])?
                || self.condition_tokens(&tokens[position + 1..])?);
        }
        if let Some(position) = tokens.iter().position(|v| *v == "and") {
            return Ok(self.condition_tokens(&tokens[..position])?
                && self.condition_tokens(&tokens[position + 1..])?);
        }
        if tokens.first().is_some_and(|v| *v == "not") {
            return Ok(!self.condition_tokens(&tokens[1..])?);
        }
        if tokens.first().is_some_and(|v| *v == "exists") && tokens.len() == 2 {
            return Ok(resolve(self.context.cwd(), &self.expand_single(tokens[1])?).exists());
        }
        if tokens.len() == 3 && ["==", "!=", "<", "<=", ">", ">="].contains(&tokens[1]) {
            let left = self.condition_operand(tokens[0])?;
            let right = self.condition_operand(tokens[2])?;
            return match tokens[1] {
                "==" => Ok(left == right),
                "!=" => Ok(left != right),
                operator => match (left, right) {
                    (Value::Integer(a), Value::Integer(b)) => Ok(match operator {
                        "<" => a < b,
                        "<=" => a <= b,
                        ">" => a > b,
                        _ => a >= b,
                    }),
                    _ => Err(Error::message("ordered comparisons require integers")),
                },
            };
        }
        if tokens.len() == 1 {
            return match self.condition_operand(tokens[0])? {
                Value::Boolean(v) => Ok(v),
                _ => Err(Error::message("condition must be boolean")),
            };
        }
        Err(Error::message("invalid condition expression"))
    }

    /// Resolves a condition operand as a variable reference or value expression.
    ///
    /// Bare, unquoted names are resolved from the current variable bindings when
    /// available; quoted operands and other expressions are evaluated directly.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let value = runtime.condition_operand("enabled")?;
    /// assert_eq!(value, Value::Boolean(true));
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    fn condition_operand(&self, source: &str) -> Result<Value> {
        if !source.starts_with(['\'', '"'])
            && let Ok(value) = lookup(&self.variables, source)
        {
            return Ok(value.clone());
        }
        self.eval_value(source)
    }

    /// Determines whether source can be interpreted as an expression.
    ///
    /// The result is `true` for existence checks, typed values, comparisons involving
    /// typed values, and boolean expressions beginning with a typed value.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert!(runtime.is_expression("42").unwrap());
    /// assert!(runtime.is_expression("name == 'Shrimp'").unwrap());
    /// assert!(!runtime.is_expression("echo hello").unwrap());
    /// ```
    fn is_expression(&self, source: &str) -> Result<bool> {
        let tokens = argument_sources(source)?;
        if tokens
            .first()
            .is_some_and(|token| *token == "exists" || *token == "not")
        {
            return Ok(true);
        }
        let typed = |token: &str| {
            token.starts_with(['\'', '"'])
                || token.starts_with("${")
                || token == "true"
                || token == "false"
                || token.parse::<i64>().is_ok()
                || lookup(&self.variables, token).is_ok()
        };
        if tokens.len() == 1 && typed(tokens[0]) {
            return Ok(true);
        }
        if tokens.len() >= 3
            && ["==", "!=", "<", "<=", ">", ">="].contains(&tokens[1])
            && (typed(tokens[0]) || typed(tokens[2]))
        {
            return Ok(true);
        }
        Ok(tokens.iter().any(|token| *token == "and" || *token == "or")
            && tokens.first().is_some_and(|token| typed(token)))
    }

    /// Builds a command invocation from source text, including environment overrides,
    /// pipelines, standard input, and output redirection.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let invocation = runtime.invocation("echo hello")?;
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the invocation syntax is invalid, a referenced input
    /// file cannot be read, or a secret value is used as a command or argument.
    fn invocation(&self, source: &str) -> Result<Invocation> {
        let (environment, source) = if let Some(rest) = source.strip_prefix("env ") {
            let (bindings, command) = split_operator(rest, " $ ").map_err(|_| {
                Error::message("environment override syntax: env NAME=VALUE $ command")
            })?;
            let bindings = argument_sources(bindings)?
                .into_iter()
                .map(|binding| {
                    let separator = scan_first_delimiter(binding, &["="])?.ok_or_else(|| {
                        Error::message(format!(
                            "environment override `{binding}` must be NAME=VALUE"
                        ))
                    })?;
                    let name = &binding[..separator.index];
                    let value = &binding[separator.index + separator.delimiter.len()..];
                    valid_env_name(name)?;
                    Ok((name.to_owned(), self.expand_single(value)?))
                })
                .collect::<Result<Vec<_>>>()?;
            (bindings, command)
        } else {
            (Vec::new(), source)
        };
        let (source, redirect) = extract_redirect(source)?;
        let (command, input) = extract_input(source)?;
        let pieces = split_pipeline(command)?;
        let mut commands = pieces.into_iter().map(|piece| {
            let mut values = words_with_secret_metadata(piece, &self.variables, &self.secrets)?
                .into_iter();
            let program = values
                .next()
                .ok_or_else(|| Error::message("empty command in pipeline"))?;
            if program.secret {
                return Err(Error::message(
                    "secret values cannot be used as command arguments; pass them with `env NAME=VALUE $ command` or stdin",
                ));
            }
            let values = values
                .map(|value| {
                    if value.secret {
                        Err(Error::message(
                            "secret values cannot be used as command arguments; pass them with `env NAME=VALUE $ command` or stdin",
                        ))
                    } else {
                        Ok(value.value)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            let mut command = cmd(program.value).args(values);
            for (name, value) in &environment {
                command = command.env(name, value);
            }
            Ok(command)
        });
        let first = commands
            .next()
            .ok_or_else(|| Error::message("empty pipeline"))??;
        let mut pipeline = first.pipeline();
        for command in commands {
            pipeline = pipeline.pipe(command?);
        }
        if let Some((inline, value)) = input {
            let bytes = if inline {
                self.expand_single(value)?.into_bytes()
            } else {
                let path = resolve(self.context.cwd(), &self.expand_single(value)?);
                std::fs::read(&path).map_err(|e| Error::io("read command stdin", Some(path), e))?
            };
            pipeline = pipeline.stdin(bytes);
        }
        let redirect = redirect
            .map(|(kind, path)| Ok::<_, Error>((kind, self.expand_single(path)?)))
            .transpose()?;
        Ok(Invocation { pipeline, redirect })
    }

    fn trace_command(&self, kind: &str, command: &str) -> Result<()> {
        if self.options.trace || self.options.dry_run {
            let expanded = split_pipeline(command)?
                .into_iter()
                .map(|part| words(part, &self.variables).map(|v| v.join(" ")))
                .collect::<Result<Vec<_>>>()?
                .join(" | ");
            eprintln!("+ {kind} {}", self.redact(expanded));
        }
        Ok(())
    }
    /// Emits a redacted trace message when tracing or dry-run mode is enabled.
    ///
    /// # Examples
    ///
    /// ```text
    /// runtime.trace("running command");
    /// ```
    fn trace(&self, message: &str) {
        if self.options.trace || self.options.dry_run {
            eprintln!("+ {}", self.redact(message.to_owned()));
        }
    }
    /// Redacts configured secret values from text.
    ///
    /// # Returns
    ///
    /// The text with occurrences of secret values replaced by redaction markers.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let redacted = runtime.redact("token=secret-value".to_owned());
    /// assert!(!redacted.contains("secret-value"));
    /// ```
    fn redact(&self, mut value: String) -> String {
        for name in &self.secrets {
            if let Some(secret) = self.variables.get(name) {
                redact_value_leaves(secret, &mut value);
            }
        }
        value
    }
}

#[derive(Clone, Copy)]
enum RedirectKind {
    Stdout,
    Append,
    Stderr,
}
struct Invocation {
    pipeline: Pipeline,
    redirect: Option<(RedirectKind, String)>,
}
impl Invocation {
    /// Determines whether the invocation redirects output to a file instead of discarding it.
    ///
    /// # Examples
    ///
    /// ```
    /// # let invocation: Invocation = todo!();
    /// assert!(invocation.changes_file());
    /// ```
    fn changes_file(&self) -> bool {
        self.redirect
            .as_ref()
            .is_some_and(|(_, path)| path != "discard")
    }
    /// Executes the invocation's command pipeline.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let output = invocation.run(&context)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// # Returns
    ///
    /// The output produced by the pipeline.
    fn run(&self, context: &Context) -> Result<CommandOutput> {
        self.pipeline.run(context)
    }
    /// Emits command output or redirects one stream to a file relative to the execution context's working directory.
    ///
    /// # Errors
    ///
    /// Returns an error if output cannot be written or a redirect file cannot be created or opened.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// invocation.finish(output, &context)?;
    /// # Ok::<(), Error>(())
    /// ```
    fn finish(&self, output: CommandOutput, context: &Context) -> Result<()> {
        match &self.redirect {
            None => emit(output),
            Some((kind, path)) => {
                if path == "discard" {
                    return match kind {
                        RedirectKind::Stdout | RedirectKind::Append => std::io::stderr()
                            .write_all(&output.stderr)
                            .map_err(|e| Error::io("write stderr", None, e)),
                        RedirectKind::Stderr => std::io::stdout()
                            .write_all(&output.stdout)
                            .map_err(|e| Error::io("write stdout", None, e)),
                    };
                }
                let path = resolve(context.cwd(), path);
                files::create_parent(&path)?;
                match kind {
                    RedirectKind::Stdout => {
                        std::fs::write(&path, &output.stdout)
                            .map_err(|e| Error::io("redirect stdout", Some(path), e))?;
                        std::io::stderr()
                            .write_all(&output.stderr)
                            .map_err(|e| Error::io("write stderr", None, e))
                    }
                    RedirectKind::Append => {
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .map_err(|e| Error::io("redirect stdout", Some(path.clone()), e))?;
                        f.write_all(&output.stdout)
                            .map_err(|e| Error::io("redirect stdout", Some(path), e))?;
                        std::io::stderr()
                            .write_all(&output.stderr)
                            .map_err(|e| Error::io("write stderr", None, e))
                    }
                    RedirectKind::Stderr => {
                        std::fs::write(&path, &output.stderr)
                            .map_err(|e| Error::io("redirect stderr", Some(path), e))?;
                        std::io::stdout()
                            .write_all(&output.stdout)
                            .map_err(|e| Error::io("write stdout", None, e))
                    }
                }
            }
        }
    }
}

fn emit(output: CommandOutput) -> Result<()> {
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(|e| Error::io("write stdout", None, e))?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(|e| Error::io("write stderr", None, e))
}
/// Resolves a path relative to a base directory while preserving absolute paths.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let resolved = resolve(Path::new("/project"), "src/main.rs");
/// assert_eq!(resolved, Path::new("/project/src/main.rs"));
/// ```
fn resolve(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    }
}

/// Generates a unique candidate path in the system temporary directory.
///
/// # Errors
///
/// Returns an error if operating-system randomness cannot be obtained.
///
/// # Examples
///
/// ```
/// let path = temporary_candidate().unwrap();
/// assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
/// assert!(path.file_name().unwrap().to_string_lossy().starts_with("shrimp-"));
/// ```
fn temporary_candidate() -> std::io::Result<PathBuf> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| std::io::Error::other(format!("obtain OS randomness: {error}")))?;
    let mut name = String::from("shrimp-");
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(std::env::temp_dir().join(name))
}

/// Creates a private temporary file or directory, retrying when a candidate path already exists.
///
/// # Examples
///
/// ```
/// let path = create_temporary_resource(false, || {
///     Ok(std::env::temp_dir().join(format!("shrimp-{}", std::process::id())))
/// })
/// .unwrap();
/// std::fs::remove_file(path).unwrap();
/// ```
fn create_temporary_resource(
    directory: bool,
    mut candidate: impl FnMut() -> std::io::Result<PathBuf>,
) -> std::result::Result<PathBuf, (Option<PathBuf>, std::io::Error)> {
    loop {
        let path = candidate().map_err(|error| (None, error))?;
        let result = if directory {
            create_private_dir(&path)
        } else {
            create_private_file(&path).map(drop)
        };
        match result {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err((Some(path), error)),
        }
    }
}

/// Creates a directory with permissions restricted to its owner.
///
/// # Examples
///
/// ```
/// let path = std::env::temp_dir().join(format!("shrimp-private-{}", std::process::id()));
/// create_private_dir(&path).unwrap();
/// assert!(path.is_dir());
/// std::fs::remove_dir(&path).unwrap();
/// ```
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

/// Creates a directory at the specified path.
///
/// # Examples
///
/// ```
/// let path = std::env::temp_dir().join("shrimp-example-dir");
/// create_private_dir(&path).unwrap();
/// assert!(path.is_dir());
/// std::fs::remove_dir(path).unwrap();
/// ```
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// Creates a new file with owner-only read and write permissions.
///
/// # Examples
///
/// ```
/// # #[cfg(unix)]
/// # {
/// use std::path::PathBuf;
///
/// let path = PathBuf::from(format!(
///     "{}/create_private_file_example_{}",
///     std::env::temp_dir().display(),
///     std::process::id()
/// ));
/// let _file = create_private_file(&path).unwrap();
/// assert!(path.exists());
/// std::fs::remove_file(path).unwrap();
/// # }
/// ```
///
/// Returns an error if the path already exists or the file cannot be created.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Creates a new file at `path`, failing if the path already exists.
///
/// # Examples
///
/// ```
/// let path = std::env::temp_dir().join(format!("shrimp-{}", std::process::id()));
/// let file = create_private_file(&path).unwrap();
/// drop(file);
/// std::fs::remove_file(path).unwrap();
/// ```
#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create_new(path)
}

#[cfg(test)]
mod temporary_tests {
    use super::create_temporary_resource;

    #[test]
    fn temporary_creation_retries_after_an_existing_candidate() {
        let root =
            std::env::temp_dir().join(format!("shrimp-temp-retry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let collision = root.join("collision");
        let available = root.join("available");
        std::fs::write(&collision, "occupied").unwrap();
        let mut candidates = [collision, available.clone()].into_iter();

        let path = create_temporary_resource(false, || Ok(candidates.next().unwrap())).unwrap();

        assert_eq!(path, available);
        assert!(path.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
/// Creates a script error associated with a source line and message.
///
/// # Examples
///
/// ```
/// let error = script_error(3, "unexpected token");
/// assert!(matches!(
///     error,
///     Error::Script { line: 3, message } if message == "unexpected token"
/// ));
/// ```
///
/// # Parameters
///
/// * `line` - The source line associated with the error.
/// * `message` - The description of the script error.
fn script_error(line: usize, message: impl Into<String>) -> Error {
    Error::Script {
        line,
        message: message.into(),
    }
}
fn attach_line(line: usize, error: Error) -> Error {
    match error {
        Error::Script { .. } => error,
        other => script_error(line, other.to_string()),
    }
}

/// Converts source text into trimmed logical lines with their starting line numbers,
/// joining lines continued with a trailing backslash and ignoring blank lines and comments.
///
/// # Examples
///
/// ```
/// let lines = logical_lines("echo hello \\\nworld\n").unwrap();
/// assert_eq!(lines, vec![(1, "echo hello world".to_string())]);
/// ```
fn logical_lines(source: &str) -> Result<Vec<(usize, String)>> {
    let mut result = Vec::new();
    let mut pending = String::new();
    let mut start = 0;
    for (index, raw) in source.lines().enumerate() {
        let line = strip_comment(raw)
            .map_err(|error| attach_line(index + 1, error))?
            .trim();
        if line.is_empty() && pending.is_empty() {
            continue;
        }
        if pending.is_empty() {
            start = index + 1
        }
        if let Some(prefix) = line.strip_suffix('\\') {
            pending.push_str(prefix);
            pending.push(' ')
        } else {
            pending.push_str(line);
            result.push((start, std::mem::take(&mut pending)))
        }
    }
    if !pending.is_empty() {
        return Err(script_error(start, "continuation at end of file"));
    }
    Ok(result)
}

/// Parses statements from the current position, stopping at the appropriate block delimiter.
///
/// # Errors
///
/// Returns an error for unexpected delimiters, unterminated nested blocks, or invalid statements.
///
/// # Examples
///
/// ```
/// let lines = vec![(1, "print hello".to_owned())];
/// let mut position = 0;
/// let (statements, ending) = parse_block(&lines, &mut position, false).unwrap();
///
/// assert_eq!(statements.len(), 1);
/// assert_eq!(ending, None);
/// assert_eq!(position, lines.len());
/// ```
fn parse_block(
    lines: &[(usize, String)],
    position: &mut usize,
    nested: bool,
) -> Result<(Vec<Statement>, Option<Ending>)> {
    let mut statements = Vec::new();
    while let Some((line, text)) = lines.get(*position) {
        if text == "end" {
            *position += 1;
            return if nested {
                Ok((statements, Some(Ending::End)))
            } else {
                Err(script_error(*line, "unexpected `end`"))
            };
        }
        if text == "else" {
            *position += 1;
            return if nested {
                Ok((statements, Some(Ending::Else)))
            } else {
                Err(script_error(*line, "unexpected `else`"))
            };
        }
        if text.starts_with("case ") {
            return if nested {
                Ok((statements, Some(Ending::Case)))
            } else {
                Err(script_error(*line, "unexpected `case`"))
            };
        }
        *position += 1;
        let kind = if let Some(value) = text.strip_prefix("match ") {
            let mut cases = Vec::new();
            let mut fallback = Vec::new();
            let (prefix, mut ending) = parse_block(lines, position, true)?;
            if !prefix.is_empty() {
                return Err(script_error(
                    *line,
                    "match statements must be inside a case",
                ));
            }
            while ending == Some(Ending::Case) {
                let (case_line, case_text) = &lines[*position];
                let pattern = case_text
                    .strip_prefix("case ")
                    .ok_or_else(|| script_error(*case_line, "match case syntax: case VALUE"))?
                    .to_owned();
                *position += 1;
                let (body, next) = parse_block(lines, position, true)?;
                cases.push((pattern, body));
                ending = next;
            }
            if ending == Some(Ending::Else) {
                let (body, end) = parse_block(lines, position, true)?;
                require_end(*line, end)?;
                fallback = body;
            } else {
                require_end(*line, ending)?;
            }
            if cases.is_empty() {
                return Err(script_error(*line, "match requires at least one case"));
            }
            StatementKind::Match {
                value: value.into(),
                cases,
                fallback,
            }
        } else if let Some(command) = text.strip_prefix("if ") {
            let (yes, ending) = parse_block(lines, position, true)?;
            let no = if ending == Some(Ending::Else) {
                let (body, end) = parse_block(lines, position, true)?;
                require_end(*line, end)?;
                body
            } else {
                require_end(*line, ending)?;
                Vec::new()
            };
            StatementKind::If {
                command: command.into(),
                yes,
                no,
            }
        } else if let Some(path) = text.strip_prefix("with cwd ") {
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::WithCwd {
                path: path.into(),
                body,
            }
        } else if let Some(rest) = text.strip_prefix("for ") {
            let (name, source) = split_operator_optional(rest, " in ")?.ok_or_else(|| {
                script_error(*line, "for syntax: for NAME in glob|lines|words VALUE")
            })?;
            valid_name(name)?;
            let values = if let Some(v) = source.strip_prefix("glob ") {
                Values::Glob(v.into())
            } else if let Some(v) = source.strip_prefix("lines ") {
                Values::Lines(v.into())
            } else if let Some(v) = source.strip_prefix("words ") {
                Values::Words(v.into())
            } else if let Some(name) = source.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
                Values::Variable(name.into())
            } else {
                return Err(script_error(
                    *line,
                    "for source must be glob, lines, words, or a list variable",
                ));
            };
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::For {
                name: name.into(),
                values,
                body,
            }
        } else if let Some(rest) = text.strip_prefix("parallel for ") {
            let (loop_part, limit) = split_operator_last(rest, " limit ")?.ok_or_else(|| {
                script_error(
                    *line,
                    "parallel for syntax: parallel for NAME in SOURCE VALUE limit COUNT",
                )
            })?;
            let (name, source) = split_operator_optional(loop_part, " in ")?.ok_or_else(|| {
                script_error(
                    *line,
                    "parallel for syntax: parallel for NAME in SOURCE VALUE limit COUNT",
                )
            })?;
            valid_name(name)?;
            let values = parse_values(*line, source)?;
            let limit = limit
                .parse()
                .map_err(|_| script_error(*line, "invalid parallel limit"))?;
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::ParallelFor {
                name: name.into(),
                values,
                limit,
                body,
            }
        } else if text == "parallel" {
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::Parallel(body)
        } else if let Some(rest) = text.strip_prefix("fn ") {
            let mut signature = rest.split_whitespace();
            let name = signature
                .next()
                .ok_or_else(|| script_error(*line, "missing function name"))?;
            valid_name(name)?;
            let parameters = signature
                .map(|v| {
                    valid_name(v)?;
                    Ok(v.to_owned())
                })
                .collect::<Result<Vec<_>>>()?;
            let unique: HashSet<_> = parameters.iter().collect();
            if unique.len() != parameters.len() {
                return Err(script_error(*line, "function parameters must be unique"));
            }
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::Function {
                name: name.into(),
                parameters,
                body,
            }
        } else {
            statements.push(parse_statement(*line, text).map_err(|e| attach_line(*line, e))?);
            continue;
        };
        statements.push(Statement { line: *line, kind });
    }
    if nested {
        Err(script_error(
            lines.last().map_or(1, |x| x.0),
            "missing `end`",
        ))
    } else {
        Ok((statements, None))
    }
}
fn require_end(line: usize, ending: Option<Ending>) -> Result<()> {
    if ending == Some(Ending::End) {
        Ok(())
    } else {
        Err(script_error(line, "missing `end`"))
    }
}

/// Parses a value source into a glob, line, word, or list-variable expression.
///
/// # Examples
///
/// ```
/// assert!(matches!(parse_values(1, "words hello"), Ok(Values::Words(_))));
/// assert!(parse_values(1, "unknown value").is_err());
/// ```
fn parse_values(line: usize, source: &str) -> Result<Values> {
    if let Some(value) = source.strip_prefix("glob ") {
        Ok(Values::Glob(value.into()))
    } else if let Some(value) = source.strip_prefix("lines ") {
        Ok(Values::Lines(value.into()))
    } else if let Some(value) = source.strip_prefix("words ") {
        Ok(Values::Words(value.into()))
    } else if let Some(name) = source.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
        Ok(Values::Variable(name.into()))
    } else {
        Err(script_error(
            line,
            "source must be glob, lines, words, or a list variable",
        ))
    }
}

/// Parses a workflow-language statement and records its source line number.
///
/// # Examples
///
/// ```
/// let statement = parse_statement(1, "$ echo hello").unwrap();
/// assert_eq!(statement.line, 1);
/// ```
fn parse_statement(line: usize, text: &str) -> Result<Statement> {
    let kind = if let Some(name) = text.strip_prefix("arg ") {
        valid_name(name)?;
        StatementKind::Input {
            name: name.into(),
            environment: false,
            secret: false,
        }
    } else if text.starts_with("secret arg ") {
        return Err(Error::message(
            "secret workflow arguments are not supported because command-line arguments are visible to shell history and process inspection; use `secret env NAME`",
        ));
    } else if let Some(name) = text.strip_prefix("secret env ") {
        valid_name(name)?;
        StatementKind::Input {
            name: name.into(),
            environment: true,
            secret: true,
        }
    } else if let Some(rest) = text.strip_prefix("let ") {
        assignment(rest, false)?
    } else if let Some(rest) = text.strip_prefix("secret ") {
        assignment(rest, true)?
    } else if let Some(path) = text.strip_prefix("include ") {
        StatementKind::Include(path.into())
    } else if let Some(rest) = text.strip_prefix("capture ") {
        let (name, command) = split_operator(rest, "<-")?;
        valid_name(name)?;
        StatementKind::Capture {
            name: name.into(),
            command: command.into(),
        }
    } else if let Some(command) = text.strip_prefix("$ ") {
        StatementKind::Run(command.into())
    } else if let Some(rest) = text.strip_prefix("env ") {
        if split_operator_optional(rest, " $ ")?.is_some() {
            StatementKind::Run(text.into())
        } else {
            valid_name(rest)?;
            StatementKind::Input {
                name: rest.into(),
                environment: true,
                secret: false,
            }
        }
    } else if let Some(rest) = text.strip_prefix("retry ") {
        let (count, command) = split_first_whitespace(rest)?
            .ok_or_else(|| script_error(line, "retry syntax: retry COUNT $ command"))?;
        StatementKind::Retry {
            attempts: count
                .parse()
                .map_err(|_| script_error(line, "invalid retry count"))?,
            command: command.strip_prefix("$ ").unwrap_or(command).into(),
        }
    } else if let Some(rest) = text.strip_prefix("timeout ") {
        let (duration, command) = split_first_whitespace(rest)?
            .ok_or_else(|| script_error(line, "timeout syntax: timeout DURATION $ command"))?;
        StatementKind::Timeout {
            duration: parse_duration(duration)?,
            command: command.strip_prefix("$ ").unwrap_or(command).into(),
        }
    } else if let Some(v) = text.strip_prefix("cd ") {
        StatementKind::Cd(v.into())
    } else if let Some(v) = text.strip_prefix("mkdir ") {
        StatementKind::Mkdir(v.into())
    } else if let Some(rest) = text.strip_prefix("write ") {
        let (path, value) = split_operator(rest, "<-")?;
        StatementKind::Write {
            path: path.into(),
            value: value.into(),
            append: false,
        }
    } else if let Some(rest) = text.strip_prefix("append ") {
        let (path, value) = split_operator(rest, "<-")?;
        StatementKind::Write {
            path: path.into(),
            value: value.into(),
            append: true,
        }
    } else if let Some(rest) = text.strip_prefix("copy ") {
        let (from, to) = split_operator(rest, "->")?;
        StatementKind::Copy {
            from: from.into(),
            to: to.into(),
        }
    } else if let Some(v) = text.strip_prefix("remove ") {
        if let Some(path) = v.strip_prefix("--recursive --force ") {
            StatementKind::RemoveTree(path.into())
        } else {
            StatementKind::Remove(v.into())
        }
    } else if let Some(rest) = text.strip_prefix("record ") {
        let (definition, fields) = split_operator_optional(rest, " fields ")?.ok_or_else(|| {
            script_error(line, "record syntax: record NAME tsv VALUE fields FIELD...")
        })?;
        let (name, value) = split_operator_optional(definition, " tsv ")?.ok_or_else(|| {
            script_error(line, "record syntax: record NAME tsv VALUE fields FIELD...")
        })?;
        valid_name(name)?;
        let pieces = fields
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if pieces.is_empty() {
            return Err(script_error(line, "record requires at least one field"));
        }
        let mut unique = HashSet::new();
        for field in &pieces {
            valid_name(field)?;
            if !unique.insert(field) {
                return Err(script_error(
                    line,
                    format!("duplicate record field `{field}`"),
                ));
            }
        }
        StatementKind::Record {
            name: name.into(),
            value: value.into(),
            fields: pieces,
        }
    } else if let Some(v) = text.strip_prefix("print ") {
        StatementKind::Print(v.into())
    } else if let Some(v) = text.strip_prefix("value ") {
        StatementKind::Value(v.into())
    } else if let Some(name) = text.strip_prefix("temp_file ") {
        valid_name(name)?;
        StatementKind::Temp {
            name: name.into(),
            directory: false,
        }
    } else if let Some(name) = text.strip_prefix("temp_dir ") {
        valid_name(name)?;
        StatementKind::Temp {
            name: name.into(),
            directory: true,
        }
    } else if let Some(rest) = text.strip_prefix("file_size ") {
        let (name, path) = split_operator(rest, "<-")?;
        valid_name(name)?;
        StatementKind::Metadata {
            name: name.into(),
            path: path.into(),
            modified: false,
        }
    } else if let Some(rest) = text.strip_prefix("modified_time ") {
        let (name, path) = split_operator(rest, "<-")?;
        valid_name(name)?;
        StatementKind::Metadata {
            name: name.into(),
            path: path.into(),
            modified: true,
        }
    } else if let Some(rest) = text.strip_prefix("call ") {
        let (target, invocation) =
            if let Some((target, invocation)) = split_operator_optional(rest, "<-")? {
                valid_name(target)?;
                (Some(target.into()), invocation)
            } else {
                (None, rest)
            };
        let (name, args) = split_first_whitespace(invocation)?.unwrap_or((invocation, ""));
        valid_name(name)?;
        StatementKind::Call {
            target,
            name: name.into(),
            arguments: args.into(),
        }
    } else {
        return Err(script_error(
            line,
            "unknown statement (commands start with `$ `)",
        ));
    };
    Ok(Statement { line, kind })
}
fn assignment(rest: &str, secret: bool) -> Result<StatementKind> {
    let (name, value) = split_operator(rest, "=")?;
    valid_name(name)?;
    Ok(StatementKind::Let {
        name: name.into(),
        value: value.into(),
        secret,
    })
}
/// Parses a duration expressed as an integer followed by `ms`, `s`, or `m`.
///
/// # Examples
///
/// ```
/// # use std::time::Duration;
/// assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
/// ```
fn parse_duration(value: &str) -> Result<Duration> {
    let index = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..index]
        .parse()
        .map_err(|_| Error::message("invalid duration"))?;
    match &value[index..] {
        "ms" => Ok(Duration::from_millis(number)),
        "s" => Ok(Duration::from_secs(number)),
        "m" => Ok(Duration::from_secs(number * 60)),
        _ => Err(Error::message("duration needs ms, s, or m suffix")),
    }
}
/// Splits a value at the specified operator.
///
/// Returns an error when the operator does not occur in the value.
///
/// # Examples
///
/// ```
/// let (left, right) = split_operator("name = value", "=").unwrap();
/// assert_eq!(left, "name ");
/// assert_eq!(right, " value");
/// ```
///
fn split_operator<'a>(value: &'a str, delimiter: &str) -> Result<(&'a str, &'a str)> {
    split_operator_optional(value, delimiter)?
        .ok_or_else(|| Error::message(format!("expected `{delimiter}`")))
}

/// Splits a value around the first occurrence of a delimiter, trimming both parts.
///
/// # Examples
///
/// ```
/// let parts = split_operator_optional("name = value", "=").unwrap();
/// assert_eq!(parts, Some(("name", "value")));
///
/// let absent = split_operator_optional("name", "=").unwrap();
/// assert_eq!(absent, None);
/// ```
fn split_operator_optional<'a>(
    value: &'a str,
    delimiter: &str,
) -> Result<Option<(&'a str, &'a str)>> {
    Ok(scan_delimiters(value, &[delimiter])?.first().map(|found| {
        (
            value[..found.index].trim(),
            value[found.index + found.delimiter.len()..].trim(),
        )
    }))
}

/// Splits a string at the last occurrence of a delimiter.
///
/// # Examples
///
/// ```
/// let parts = split_operator_last("left = middle = right", "=").unwrap();
/// assert_eq!(parts, Some(("left = middle", "right")));
/// ```
fn split_operator_last<'a>(value: &'a str, delimiter: &str) -> Result<Option<(&'a str, &'a str)>> {
    Ok(scan_delimiters(value, &[delimiter])?.last().map(|found| {
        (
            value[..found.index].trim(),
            value[found.index + found.delimiter.len()..].trim(),
        )
    }))
}

/// Splits a string at its first whitespace character and trims whitespace from the remainder.
///
/// # Examples
///
/// ```
/// assert_eq!(
///     split_first_whitespace("hello   world").unwrap(),
///     Some(("hello", "world")),
/// );
/// assert_eq!(split_first_whitespace("hello").unwrap(), None);
/// ```
fn split_first_whitespace(value: &str) -> Result<Option<(&str, &str)>> {
    Ok(
        scan_first_delimiter(value, &[" ", "\t", "\r", "\n"])?.map(|separator| {
            (
                &value[..separator.index],
                value[separator.index + separator.delimiter.len()..].trim_start(),
            )
        }),
    )
}

#[derive(Clone, Copy)]
struct DelimiterMatch<'a> {
    index: usize,
    delimiter: &'a str,
}

/// Finds configured delimiters outside quoted text in a single pass.
///
/// When delimiters share a prefix, the longest matching delimiter is selected.
///
/// # Examples
///
/// ```
/// let matches = scan_delimiters("value ${name}", &["$", "${"])?;
/// assert_eq!(matches.len(), 1);
/// # Ok::<(), _>(())
/// ```
fn scan_delimiters<'a>(source: &str, delimiters: &[&'a str]) -> Result<Vec<DelimiterMatch<'a>>> {
    scan_delimiters_with_mode(source, delimiters, false)
}

/// Finds the first matching delimiter in source while respecting quoted sections.
///
/// # Examples
///
/// ```
/// let result = scan_first_delimiter("echo \"a|b\" | cat", &["|"])?;
/// assert!(result.is_some());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Returns
///
/// The first delimiter match, or `None` when no delimiter is found.
fn scan_first_delimiter<'a>(
    source: &str,
    delimiters: &[&'a str],
) -> Result<Option<DelimiterMatch<'a>>> {
    Ok(scan_delimiters_with_mode(source, delimiters, true)?
        .into_iter()
        .next())
}

/// Finds delimiters outside quoted and escaped portions of a source string.
///
/// When multiple delimiters start at the same position, the longest matching delimiter is selected.
/// Scanning can stop after the first match when `stop_at_first` is `true`.
///
/// # Arguments
///
/// * `source` - The text to scan.
/// * `delimiters` - The delimiter strings to recognize.
/// * `stop_at_first` - Whether to return only the first match.
///
/// # Errors
///
/// Returns an error when the source contains an unclosed quote.
///
/// # Examples
///
/// ```
/// let matches = scan_delimiters_with_mode("a||b|c", &["|", "||"], false).unwrap();
/// assert_eq!(matches.len(), 2);
/// ```
fn scan_delimiters_with_mode<'a>(
    source: &str,
    delimiters: &[&'a str],
    stop_at_first: bool,
) -> Result<Vec<DelimiterMatch<'a>>> {
    let mut matches = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut skip_until = 0;
    for (index, character) in source.char_indices() {
        if index < skip_until {
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if character == '\'' || character == '"' {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none()
            && let Some(delimiter) = delimiters
                .iter()
                .copied()
                .filter(|delimiter| source[index..].starts_with(delimiter))
                .max_by_key(|delimiter| delimiter.len())
        {
            matches.push(DelimiterMatch { index, delimiter });
            if stop_at_first {
                return Ok(matches);
            }
            skip_until = index + delimiter.len();
        }
    }
    if quote.is_some() {
        return Err(Error::message("unclosed quote"));
    }
    Ok(matches)
}
/// Validates that a name is nonempty and contains only ASCII letters, digits, or underscores.
///
/// # Examples
///
/// ```
/// assert!(valid_name("variable_1").is_ok());
/// assert!(valid_name("invalid-name").is_err());
/// ```
///
/// # Arguments
///
/// * `name` - The name to validate.
///
/// # Errors
///
/// Returns an error when `name` is empty or contains any other character.
fn valid_name(name: &str) -> Result<()> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(Error::message(format!("invalid name `{name}`")))
    }
}

/// Validates an environment variable name.
///
/// Names must be nonempty, begin with a letter or underscore, and contain only
/// ASCII letters, digits, and underscores.
///
/// # Examples
///
/// ```
/// assert!(valid_env_name("BUILD_ID").is_ok());
/// assert!(valid_env_name("9BUILD_ID").is_err());
/// ```
fn valid_env_name(name: &str) -> Result<()> {
    if !name.is_empty()
        && name.chars().enumerate().all(|(index, c)| {
            c == '_' || c.is_ascii_alphanumeric() && (index > 0 || !c.is_ascii_digit())
        })
    {
        Ok(())
    } else {
        Err(Error::message(format!("invalid environment name `{name}`")))
    }
}

/// Extracts a standard-input source from a command after output redirection has been removed.
///
/// # Errors
///
/// Returns an error if the input redirection uses unsupported `<<` syntax or has no source value.
///
/// # Examples
///
/// ```
/// let (command, input) = extract_input("cat < input.txt").unwrap();
/// assert_eq!(command, "cat");
/// assert_eq!(input, Some((false, "input.txt")));
/// ```
fn extract_input(line: &str) -> Result<(&str, Option<(bool, &str)>)> {
    let found = scan_delimiters(line, &["<", "<-", "<<", "<<<"])?
        .into_iter()
        .find(|found| found.delimiter != "<-");
    if let Some(found) = found {
        if found.delimiter == "<<" {
            return Err(Error::message(
                "stdin redirection syntax: `< FILE` or `<<< VALUE`",
            ));
        }
        let value = line[found.index + found.delimiter.len()..].trim();
        if value.is_empty() {
            return Err(Error::message("stdin redirection needs a file or value"));
        }
        return Ok((
            line[..found.index].trim(),
            Some((found.delimiter == "<<<", value)),
        ));
    }
    Ok((line, None))
}

/// Extracts the first output or error redirection from a command line.
///
/// Returns the command text and, when present, the redirection kind and target path.
/// An error is returned when a redirection operator has no target path.
///
/// # Examples
///
/// ```
/// let (command, redirect) = extract_redirect("echo hello > output.txt").unwrap();
/// assert_eq!(command, "echo hello");
/// assert!(matches!(
///     redirect,
///     Some((RedirectKind::Stdout, "output.txt"))
/// ));
/// ```
fn extract_redirect(line: &str) -> Result<(&str, Option<(RedirectKind, &str)>)> {
    if let Some(found) = scan_delimiters(line, &[">", ">>", "2>"])?
        .into_iter()
        .next()
    {
        let kind = match found.delimiter {
            "2>" => RedirectKind::Stderr,
            ">>" => RedirectKind::Append,
            _ => RedirectKind::Stdout,
        };
        let path = line[found.index + found.delimiter.len()..].trim();
        if path.is_empty() {
            return Err(Error::message("redirection needs a path"));
        }
        Ok((line[..found.index].trim(), Some((kind, path))))
    } else {
        Ok((line, None))
    }
}
/// Removes the comment delimiter and following text from a line while preserving delimiters inside quotes.
///
/// # Examples
///
/// ```
/// assert_eq!(
///     strip_comment(r#"echo "value # kept" # comment"#).unwrap(),
///     r#"echo "value # kept" "#
/// );
/// ```
fn strip_comment(line: &str) -> Result<&str> {
    Ok(scan_first_delimiter(line, &["#"])?.map_or(line, |found| &line[..found.index]))
}
/// Splits a command line into pipeline segments while preserving delimiters inside quoted text.
///
/// # Examples
///
/// ```
/// let segments = split_pipeline("echo hello | tr a-z A-Z").unwrap();
/// assert_eq!(segments, ["echo hello", "tr a-z A-Z"]);
/// ```
///
/// # Errors
///
/// Returns an error when the line contains an invalid delimiter sequence.
fn split_pipeline(line: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut start = 0;
    for found in scan_delimiters(line, &["|"])? {
        result.push(line[start..found.index].trim());
        start = found.index + found.delimiter.len();
    }
    result.push(line[start..].trim());
    Ok(result)
}
/// Extracts the root name from a variable path.
///
/// The root ends before the first `.`, `[`, or `]` delimiter.
///
/// # Examples
///
/// ```
/// assert_eq!(secret_root("config.database"), Ok("config"));
/// assert_eq!(secret_root("items[0]"), Ok("items"));
/// assert_eq!(secret_root("token"), Ok("token"));
/// ```
fn secret_root(path: &str) -> Result<&str> {
    Ok(scan_first_delimiter(path, &[".", "[", "]"])?
        .map_or(path, |delimiter| &path[..delimiter.index]))
}

/// Resolves a variable path through record fields and list indices.
///
/// # Errors
///
/// Returns an error if the base variable is undefined, a record field is missing,
/// a list index is invalid or out of bounds, or the path is malformed.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
///
/// let mut variables = HashMap::new();
/// variables.insert(
///     "config".to_string(),
///     Value::Record(HashMap::from([
///         ("names".to_string(), Value::List(vec![
///             Value::String("shrimp".to_string()),
///         ])),
///     ])),
/// );
///
/// assert!(matches!(
///     lookup(&variables, "config.names[0]"),
///     Ok(Value::String(name)) if name == "shrimp"
/// ));
/// ```
fn lookup<'a>(variables: &'a HashMap<String, Value>, path: &str) -> Result<&'a Value> {
    let first = scan_first_delimiter(path, &[".", "[", "]"])?;
    let (base, mut rest) = first.map_or((path, ""), |found| {
        (&path[..found.index], &path[found.index..])
    });
    let mut value = variables
        .get(base)
        .ok_or_else(|| Error::message(format!("undefined variable `{base}`")))?;
    while !rest.is_empty() {
        if let Some(field) = rest.strip_prefix('.') {
            let end = scan_first_delimiter(field, &[".", "[", "]"])?
                .map_or(field.len(), |found| found.index);
            let key = &field[..end];
            value = match value {
                Value::Record(values) => values.get(key),
                _ => None,
            }
            .ok_or_else(|| Error::message(format!("missing record field `{key}`")))?;
            rest = &field[end..];
        } else if let Some(index) = rest.strip_prefix('[') {
            let end = scan_first_delimiter(index, &[".", "[", "]"])?
                .filter(|found| found.delimiter == "]")
                .map(|found| found.index)
                .ok_or_else(|| Error::message("unclosed list index"))?;
            let number: usize = index[..end]
                .parse()
                .map_err(|_| Error::message("list index must be a non-negative integer"))?;
            value = match value {
                Value::List(values) => values.get(number),
                _ => None,
            }
            .ok_or_else(|| Error::message(format!("list index {number} is out of bounds")))?;
            rest = &index[end + 1..];
        } else {
            return Err(Error::message(format!("invalid value path `{path}`")));
        }
    }
    Ok(value)
}

/// Splits an argument string at unquoted whitespace while preserving quoted segments.
///
/// # Errors
///
/// Returns an error when the input contains an unclosed single or double quote.
///
/// # Examples
///
/// ```
/// let arguments = argument_sources(r#"build "release candidate" --target x86"#).unwrap();
/// assert_eq!(arguments, ["build", r#""release candidate""#, "--target", "x86"]);
/// ```
fn argument_sources(source: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            start.get_or_insert(index);
            continue;
        }
        if character == '\'' || character == '"' {
            start.get_or_insert(index);
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
        } else if character.is_whitespace() && quote.is_none() {
            if let Some(begin) = start.take() {
                result.push(&source[begin..index]);
            }
        } else {
            start.get_or_insert(index);
        }
    }
    if quote.is_some() {
        return Err(Error::message("unclosed quote"));
    }
    if let Some(begin) = start {
        result.push(&source[begin..]);
    }
    Ok(result)
}

struct ExpandedWord {
    value: String,
    secret: bool,
}

/// Expands source text into words using the supplied variable values.
///
/// # Examples
///
/// ```
/// let variables = std::collections::HashMap::new();
/// let result = words("echo hello", &variables).unwrap();
///
/// assert_eq!(result, vec!["echo", "hello"]);
/// ```
///
/// # Arguments
///
/// * `source` - The text to split and expand.
/// * `variables` - The values available for interpolation.
///
/// # Returns
///
/// The expanded words, or an error if expansion fails.
fn words(source: &str, variables: &HashMap<String, Value>) -> Result<Vec<String>> {
    Ok(
        words_with_secret_metadata(source, variables, &HashSet::new())?
            .into_iter()
            .map(|word| word.value)
            .collect(),
    )
}

/// Splits source text into expanded words and marks words containing secret variables.
///
/// Variables are interpolated using `${name}` syntax, while quotes and backslash
/// escapes control word boundaries and character expansion. A word is marked
/// secret when it contains a variable whose root name is present in `secrets`.
///
/// # Errors
///
/// Returns an error for unclosed variable interpolations or quotes, invalid
/// variable expressions, or values that cannot be converted to scalar text.
///
/// # Examples
///
/// ```
/// let variables = std::collections::HashMap::from([
///     ("TOKEN".to_string(), Value::String("secret".to_string())),
/// ]);
/// let secrets = std::collections::HashSet::from(["TOKEN".to_string()]);
///
/// let words = words_with_secret_metadata("echo ${TOKEN}", &variables, &secrets).unwrap();
///
/// assert_eq!(words[0].value, "echo");
/// assert_eq!(words[1].value, "secret");
/// assert!(words[1].secret);
/// ```
fn words_with_secret_metadata(
    source: &str,
    variables: &HashMap<String, Value>,
    secrets: &HashSet<String>,
) -> Result<Vec<ExpandedWord>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut word_is_secret = false;
    let mut quote = None;
    let mut started = false;
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote != Some('\'') => {
                if let Some(next) = chars.next() {
                    started = true;
                    word.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    })
                }
            }
            '\'' | '"' => {
                if quote == Some(c) {
                    quote = None
                } else if quote.is_none() {
                    started = true;
                    quote = Some(c)
                } else {
                    word.push(c)
                }
            }
            c if c.is_whitespace() && quote.is_none() => {
                if started {
                    result.push(ExpandedWord {
                        value: std::mem::take(&mut word),
                        secret: word_is_secret,
                    });
                    started = false;
                    word_is_secret = false;
                }
            }
            '$' if quote != Some('\'') && chars.peek() == Some(&'{') => {
                started = true;
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    } else {
                        name.push(next)
                    }
                }
                if !closed {
                    return Err(Error::message("unclosed variable interpolation"));
                }
                let root = scan_first_delimiter(&name, &[".", "[", "]"])?
                    .map_or(name.as_str(), |found| &name[..found.index]);
                word_is_secret |= secrets.contains(root);
                word.push_str(&lookup(variables, &name)?.scalar()?)
            }
            other => {
                started = true;
                word.push(other)
            }
        }
    }
    if quote.is_some() {
        return Err(Error::message("unclosed quote"));
    }
    if started {
        result.push(ExpandedWord {
            value: word,
            secret: word_is_secret,
        })
    }
    Ok(result)
}

/// Redacts secret values from trace output, including values nested in lists and records.
///
/// Nonempty strings and distinctive integers are replaced with `[REDACTED]`.
///
/// # Arguments
///
/// * `secret` - The secret value whose scalar leaves are redacted.
/// * `output` - The trace text to update.
///
/// # Examples
///
/// ```
/// let secret = Value::String("token".to_owned());
/// let mut output = "Authorization: token".to_owned();
///
/// redact_value_leaves(&secret, &mut output);
///
/// assert_eq!(output, "Authorization: [REDACTED]");
/// ```
fn redact_value_leaves(secret: &Value, output: &mut String) {
    match secret {
        Value::String(value) if !value.is_empty() => *output = output.replace(value, "[REDACTED]"),
        // Boolean spellings and short integers are too common to safely use as
        // substring redaction keys; they corrupt unrelated trace text.
        Value::Boolean(_) => {}
        Value::Integer(value) => redact_distinctive_integer(*value, output),
        Value::List(values) => {
            for value in values {
                redact_value_leaves(value, output);
            }
        }
        Value::Record(values) => {
            for value in values.values() {
                redact_value_leaves(value, output);
            }
        }
        Value::Missing | Value::String(_) => {}
    }
}

/// Replaces standalone occurrences of a distinctive integer with `[REDACTED]`.
///
/// Integers with fewer than four digits are left unchanged, as are occurrences
/// embedded within larger digit sequences.
///
/// # Examples
///
/// ```
/// let mut output = "token 123456 and code 12".to_string();
/// redact_distinctive_integer(123456, &mut output);
/// assert_eq!(output, "token [REDACTED] and code 12");
/// ```
///
/// # Arguments
///
/// * `secret` - The integer value to redact.
/// * `output` - The text in which matching occurrences are replaced.
fn redact_distinctive_integer(secret: i64, output: &mut String) {
    let rendered = secret.to_string();
    if rendered.trim_start_matches('-').len() < 4 {
        return;
    }
    let mut redacted = String::with_capacity(output.len());
    let mut start = 0;
    for (index, _) in output.match_indices(&rendered) {
        let before_is_digit = !rendered.starts_with('-')
            && output[..index]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_digit());
        let end = index + rendered.len();
        let after_is_digit = output[end..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit());
        if before_is_digit || after_is_digit {
            continue;
        }
        redacted.push_str(&output[start..index]);
        redacted.push_str("[REDACTED]");
        start = end;
    }
    redacted.push_str(&output[start..]);
    *output = redacted;
}
