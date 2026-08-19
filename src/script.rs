//! Parser and interpreter for Shrimp's portable workflow language.

use crate::{CommandOutput, Context, Error, Pipeline, Result, cmd, files};
use glob::glob;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

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

fn include_wait_would_cycle(
    registry: &IncludeRegistry,
    current: thread::ThreadId,
    mut owner: thread::ThreadId,
) -> bool {
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

    pub fn run_with_options(
        &self,
        context: &Context,
        options: ScriptOptions,
    ) -> Result<ScriptReport> {
        let variables = context
            .env()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    Value::String(value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let mut runtime = Runtime {
            context: context.clone(),
            variables,
            secrets: HashSet::new(),
            functions: HashMap::new(),
            call_depth: 0,
            parallel_depth: 0,
            options,
            report: ScriptReport::default(),
            source_dirs: vec![context.cwd().to_owned()],
            includes: Arc::new((Mutex::new(IncludeRegistry::default()), Condvar::new())),
            last_value: Value::Missing,
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
    last_value: Value,
    temporary_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl Runtime {
    fn execute(&mut self, statements: &[Statement]) -> Result<()> {
        for statement in statements {
            self.execute_one(statement)
                .map_err(|error| attach_line(statement.line, error))?;
        }
        Ok(())
    }

    fn execute_one(&mut self, statement: &Statement) -> Result<()> {
        self.last_value = Value::Missing;
        match &statement.kind {
            StatementKind::Let {
                name,
                value,
                secret,
            } => {
                let value = self.eval_value(value)?;
                self.variables.insert(name.clone(), value);
                self.last_value = self.variables[name].clone();
                if *secret {
                    self.secrets.insert(name.clone());
                }
            }
            StatementKind::Capture { name, command } => {
                self.trace_command("capture", command)?;
                if self.options.dry_run {
                    self.variables
                        .insert(name.clone(), Value::String(String::new()));
                } else {
                    let invocation = self.invocation(command)?;
                    if invocation.redirect.is_some() {
                        return Err(Error::message(
                            "capture cannot be combined with redirection",
                        ));
                    }
                    let output = invocation.run(&self.context)?;
                    self.report.commands_run += 1;
                    self.variables.insert(
                        name.clone(),
                        Value::String(
                            output
                                .stdout_string()?
                                .trim_end_matches(['\r', '\n'])
                                .to_owned(),
                        ),
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
                self.variables.insert(name.clone(), value.clone());
                self.last_value = value;
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
                let value = self.call(name, arguments)?;
                if let Some(target) = target {
                    self.variables.insert(target.clone(), value.clone());
                }
                self.last_value = value;
            }
            StatementKind::Include(path) => self.include(path)?,
            StatementKind::Value(source) => self.last_value = self.eval_value(source)?,
            StatementKind::Temp { name, directory } => self.create_temporary(name, *directory)?,
            StatementKind::Metadata {
                name,
                path,
                modified,
            } => self.metadata(name, path, *modified)?,
        }
        Ok(())
    }

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

    fn call(&mut self, name: &str, arguments: &str) -> Result<Value> {
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
            .map(|argument| self.eval_value(argument))
            .collect::<Result<Vec<_>>>()?;
        if arguments.len() != definition.parameters.len() {
            return Err(Error::message(format!(
                "function `{name}` expects {} arguments, got {}",
                definition.parameters.len(),
                arguments.len()
            )));
        }
        let saved = self.variables.clone();
        for (parameter, argument) in definition.parameters.into_iter().zip(arguments) {
            self.variables.insert(parameter, argument);
        }
        self.call_depth += 1;
        let saved_value = self.last_value.clone();
        self.source_dirs.push(definition.source_dir);
        let result = self
            .execute(&definition.body)
            .map(|()| self.last_value.clone());
        self.source_dirs.pop();
        self.call_depth -= 1;
        self.variables = saved;
        self.last_value = saved_value;
        result
    }

    fn create_temporary(&mut self, name: &str, directory: bool) -> Result<()> {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("shrimp-{}-{id}", std::process::id()));
        self.trace(&format!(
            "{} {}",
            if directory { "temp_dir" } else { "temp_file" },
            path.display()
        ));
        if !self.options.dry_run {
            if directory {
                create_private_dir(&path)
                    .map_err(|e| Error::io("create temporary directory", Some(path.clone()), e))?;
            } else {
                create_private_file(&path)
                    .map_err(|e| Error::io("create temporary file", Some(path.clone()), e))?;
            }
            self.temporary_paths
                .lock()
                .expect("temporary path registry poisoned")
                .push(path.clone());
        }
        let value = Value::String(path.to_string_lossy().into_owned());
        self.variables.insert(name.into(), value.clone());
        self.last_value = value;
        Ok(())
    }

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

    fn metadata(&mut self, name: &str, source: &str, modified: bool) -> Result<()> {
        let path = resolve(self.context.cwd(), &self.expand_single(source)?);
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
        self.variables.insert(name.into(), value.clone());
        self.last_value = value;
        Ok(())
    }

    fn include(&mut self, source: &str) -> Result<()> {
        let requested = self.expand_single(source)?;
        let base = self
            .source_dirs
            .last()
            .expect("runtime always has a source directory");
        let path = resolve(base, &requested);
        let path = std::fs::canonicalize(&path)
            .map_err(|error| Error::io("resolve include", Some(path), error))?;
        let current_thread = thread::current().id();
        let includes = Arc::clone(&self.includes);
        let (lock, changed) = &*includes;
        loop {
            let mut registry = lock.lock().expect("include registry poisoned");
            if let Some(exports) = registry.loaded.get(&path).cloned() {
                drop(registry);
                self.variables.extend(exports.variables);
                self.functions.extend(exports.functions);
                self.secrets.extend(exports.secrets);
                return Ok(());
            }
            if let Some(owner) = registry.active.get(&path).copied() {
                if owner == current_thread
                    || include_wait_would_cycle(&registry, current_thread, owner)
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
        let variables_before = self.variables.clone();
        let functions_before = self.functions.clone();
        let secrets_before = self.secrets.clone();
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
        let mut registry = lock.lock().expect("include registry poisoned");
        registry.active.remove(&path);
        if result.is_ok() {
            let variables = self
                .variables
                .iter()
                .filter(|(name, value)| variables_before.get(*name) != Some(*value))
                .map(|(name, value)| (name.clone(), value.clone()))
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

    fn condition(&self, source: &str) -> Result<bool> {
        let tokens = argument_sources(source)?;
        self.condition_tokens(&tokens)
    }

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

    fn condition_operand(&self, source: &str) -> Result<Value> {
        if !source.starts_with(['\'', '"'])
            && let Ok(value) = lookup(&self.variables, source)
        {
            return Ok(value.clone());
        }
        self.eval_value(source)
    }

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

    fn invocation(&self, source: &str) -> Result<Invocation> {
        let (environment, source) = if let Some(rest) = source.strip_prefix("env ") {
            let (bindings, command) = split_operator(rest, " $ ").map_err(|_| {
                Error::message("environment override syntax: env NAME=VALUE $ command")
            })?;
            let bindings = words(bindings, &self.variables)?
                .into_iter()
                .map(|binding| {
                    let (name, value) = binding.split_once('=').ok_or_else(|| {
                        Error::message(format!(
                            "environment override `{binding}` must be NAME=VALUE"
                        ))
                    })?;
                    valid_env_name(name)?;
                    Ok((name.to_owned(), value.to_owned()))
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
            let mut values = words(piece, &self.variables)?.into_iter();
            let program = values
                .next()
                .ok_or_else(|| Error::message("empty command in pipeline"))?;
            let mut command = cmd(program).args(values);
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
    fn trace(&self, message: &str) {
        if self.options.trace || self.options.dry_run {
            eprintln!("+ {}", self.redact(message.to_owned()));
        }
    }
    fn redact(&self, mut value: String) -> String {
        for name in &self.secrets {
            if let Some(secret) = self.variables.get(name).and_then(|v| v.scalar().ok())
                && !secret.is_empty()
            {
                value = value.replace(&secret, "[REDACTED]");
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
    fn changes_file(&self) -> bool {
        self.redirect
            .as_ref()
            .is_some_and(|(_, path)| path != "discard")
    }
    fn run(&self, context: &Context) -> Result<CommandOutput> {
        self.pipeline.run(context)
    }
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
fn resolve(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create_new(path)
}
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

fn parse_statement(line: usize, text: &str) -> Result<Statement> {
    let kind = if let Some(rest) = text.strip_prefix("let ") {
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
    } else if text.starts_with("env ") && text.contains(" $ ") {
        StatementKind::Run(text.into())
    } else if let Some(rest) = text.strip_prefix("retry ") {
        let (count, command) = rest
            .split_once(' ')
            .ok_or_else(|| script_error(line, "retry syntax: retry COUNT $ command"))?;
        StatementKind::Retry {
            attempts: count
                .parse()
                .map_err(|_| script_error(line, "invalid retry count"))?,
            command: command.strip_prefix("$ ").unwrap_or(command).into(),
        }
    } else if let Some(rest) = text.strip_prefix("timeout ") {
        let (duration, command) = rest
            .split_once(' ')
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
        let (name, args) = invocation.split_once(' ').unwrap_or((invocation, ""));
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
fn split_operator<'a>(value: &'a str, delimiter: &str) -> Result<(&'a str, &'a str)> {
    split_operator_optional(value, delimiter)?
        .ok_or_else(|| Error::message(format!("expected `{delimiter}`")))
}

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

fn split_operator_last<'a>(value: &'a str, delimiter: &str) -> Result<Option<(&'a str, &'a str)>> {
    Ok(scan_delimiters(value, &[delimiter])?.last().map(|found| {
        (
            value[..found.index].trim(),
            value[found.index + found.delimiter.len()..].trim(),
        )
    }))
}

#[derive(Clone, Copy)]
struct DelimiterMatch<'a> {
    index: usize,
    delimiter: &'a str,
}

/// Finds configurable delimiters outside quotes in one quote-aware pass.
/// The longest matching delimiter wins when delimiters share a prefix.
fn scan_delimiters<'a>(source: &str, delimiters: &[&'a str]) -> Result<Vec<DelimiterMatch<'a>>> {
    scan_delimiters_with_mode(source, delimiters, false)
}

fn scan_first_delimiter<'a>(
    source: &str,
    delimiters: &[&'a str],
) -> Result<Option<DelimiterMatch<'a>>> {
    Ok(scan_delimiters_with_mode(source, delimiters, true)?
        .into_iter()
        .next())
}

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
        if character == '\\' {
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
fn valid_name(name: &str) -> Result<()> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(Error::message(format!("invalid name `{name}`")))
    }
}

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

/// Extracts one stdin source after output redirection has been removed.
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
fn strip_comment(line: &str) -> Result<&str> {
    Ok(scan_first_delimiter(line, &["#"])?.map_or(line, |found| &line[..found.index]))
}
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
fn lookup<'a>(variables: &'a HashMap<String, Value>, path: &str) -> Result<&'a Value> {
    let (base, mut rest) = path
        .split_once(['.', '['])
        .map_or((path, ""), |(a, _)| (a, &path[a.len()..]));
    let mut value = variables
        .get(base)
        .ok_or_else(|| Error::message(format!("undefined variable `{base}`")))?;
    while !rest.is_empty() {
        if let Some(field) = rest.strip_prefix('.') {
            let end = field.find(['.', '[']).unwrap_or(field.len());
            let key = &field[..end];
            value = match value {
                Value::Record(values) => values.get(key),
                _ => None,
            }
            .ok_or_else(|| Error::message(format!("missing record field `{key}`")))?;
            rest = &field[end..];
        } else if let Some(index) = rest.strip_prefix('[') {
            let end = index
                .find(']')
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

fn words(source: &str, variables: &HashMap<String, Value>) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
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
                    result.push(std::mem::take(&mut word));
                    started = false
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
        result.push(word)
    }
    Ok(result)
}
