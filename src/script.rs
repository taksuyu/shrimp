//! Parser and interpreter for Shrimp's portable workflow language.

use crate::{CommandOutput, Context, Error, Pipeline, Result, cmd, files};
use glob::glob;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

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
    Print(String),
    If {
        command: String,
        yes: Vec<Statement>,
        no: Vec<Statement>,
    },
    For {
        name: String,
        values: Values,
        body: Vec<Statement>,
    },
    Parallel(Vec<Statement>),
    Function {
        name: String,
        parameters: Vec<String>,
        body: Vec<Statement>,
    },
    Call {
        name: String,
        arguments: String,
    },
}

#[derive(Clone, Debug)]
enum Values {
    Glob(String),
    Lines(String),
    Words(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    End,
    Else,
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
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let mut runtime = Runtime {
            context: context.clone(),
            variables,
            secrets: HashSet::new(),
            functions: HashMap::new(),
            options,
            report: ScriptReport::default(),
        };
        runtime.execute(&self.statements)?;
        Ok(runtime.report)
    }
}

#[derive(Clone)]
struct Runtime {
    context: Context,
    variables: HashMap<String, String>,
    secrets: HashSet<String>,
    functions: HashMap<String, (Vec<String>, Vec<Statement>)>,
    options: ScriptOptions,
    report: ScriptReport,
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
        match &statement.kind {
            StatementKind::Let {
                name,
                value,
                secret,
            } => {
                let value = self.expand_single(value)?;
                self.variables.insert(name.clone(), value);
                if *secret {
                    self.secrets.insert(name.clone());
                }
            }
            StatementKind::Capture { name, command } => {
                self.trace_command("capture", command)?;
                if self.options.dry_run {
                    self.variables.insert(name.clone(), String::new());
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
                        output
                            .stdout_string()?
                            .trim_end_matches(['\r', '\n'])
                            .to_owned(),
                    );
                }
            }
            StatementKind::Run(command) => {
                self.trace_command("run", command)?;
                if !self.options.dry_run {
                    let invocation = self.invocation(command)?;
                    let output = invocation.pipeline.run(&self.context)?;
                    self.report.commands_run += 1;
                    invocation.finish(output, &self.context)?;
                    self.report.files_changed += usize::from(invocation.redirect.is_some());
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
                                self.report.files_changed +=
                                    usize::from(invocation.redirect.is_some());
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
                    self.report.files_changed += usize::from(invocation.redirect.is_some());
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
                    if *append {
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .map_err(|e| Error::io("append", Some(path.clone()), e))?;
                        file.write_all(value.as_bytes())
                            .map_err(|e| Error::io("append", Some(path), e))?;
                    } else {
                        files::write_atomic(path, value).run(&Context::new("/"))?;
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
            StatementKind::Print(value) => {
                let value = self.expand_single(value)?;
                self.trace(&format!("print {}", self.redact(value.clone())));
                if !self.options.dry_run {
                    println!("{value}");
                }
            }
            StatementKind::If { command, yes, no } => {
                self.trace_command("if", command)?;
                let success = if self.options.dry_run {
                    true
                } else {
                    self.report.commands_run += 1;
                    self.invocation(command)?
                        .pipeline
                        .is_success(&self.context)?
                };
                self.execute(if success { yes } else { no })?;
            }
            StatementKind::For { name, values, body } => {
                for value in self.values(values)? {
                    self.variables.insert(name.clone(), value);
                    self.execute(body)?;
                }
                self.variables.remove(name);
            }
            StatementKind::Parallel(branches) => self.parallel(branches)?,
            StatementKind::Function {
                name,
                parameters,
                body,
            } => {
                self.functions
                    .insert(name.clone(), (parameters.clone(), body.clone()));
            }
            StatementKind::Call { name, arguments } => self.call(name, arguments)?,
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
        }
    }

    fn parallel(&mut self, branches: &[Statement]) -> Result<()> {
        self.trace(&format!("parallel {} branches", branches.len()));
        if self.options.dry_run {
            for branch in branches {
                self.execute(std::slice::from_ref(branch))?;
            }
            return Ok(());
        }
        let handles: Vec<_> = branches
            .iter()
            .cloned()
            .map(|branch| {
                let mut runtime = self.clone();
                thread::spawn(move || {
                    runtime.report = ScriptReport::default();
                    runtime.execute(&[branch])?;
                    Ok::<_, Error>(runtime.report)
                })
            })
            .collect();
        for handle in handles {
            let report = handle
                .join()
                .map_err(|_| Error::message("parallel branch panicked"))??;
            self.report.commands_run += report.commands_run;
            self.report.files_changed += report.files_changed;
        }
        Ok(())
    }

    fn call(&mut self, name: &str, arguments: &str) -> Result<()> {
        let (parameters, body) = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| Error::message(format!("undefined function `{name}`")))?;
        let arguments = words(arguments, &self.variables)?;
        if arguments.len() != parameters.len() {
            return Err(Error::message(format!(
                "function `{name}` expects {} arguments, got {}",
                parameters.len(),
                arguments.len()
            )));
        }
        let saved = self.variables.clone();
        for (parameter, argument) in parameters.into_iter().zip(arguments) {
            self.variables.insert(parameter, argument);
        }
        let result = self.execute(&body);
        self.variables = saved;
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

    fn invocation(&self, source: &str) -> Result<Invocation> {
        let (command, redirect) = extract_redirect(source)?;
        let pieces = split_pipeline(command)?;
        let mut commands = pieces.into_iter().map(|piece| {
            let mut values = words(piece, &self.variables)?.into_iter();
            let program = values
                .next()
                .ok_or_else(|| Error::message("empty command in pipeline"))?;
            Ok(cmd(program).args(values))
        });
        let first = commands
            .next()
            .ok_or_else(|| Error::message("empty pipeline"))??;
        let mut pipeline = first.pipeline();
        for command in commands {
            pipeline = pipeline.pipe(command?);
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
            if let Some(secret) = self.variables.get(name)
                && !secret.is_empty()
            {
                value = value.replace(secret, "[REDACTED]");
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
    fn run(&self, context: &Context) -> Result<CommandOutput> {
        self.pipeline.run(context)
    }
    fn finish(&self, output: CommandOutput, context: &Context) -> Result<()> {
        match &self.redirect {
            None => emit(output),
            Some((kind, path)) => {
                let path = resolve(context.cwd(), path);
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
        let line = strip_comment(raw).trim();
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
        *position += 1;
        let kind = if let Some(command) = text.strip_prefix("if ") {
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
            let (name, source) = rest.split_once(" in ").ok_or_else(|| {
                script_error(*line, "for syntax: for NAME in glob|lines|words VALUE")
            })?;
            valid_name(name)?;
            let values = if let Some(v) = source.strip_prefix("glob ") {
                Values::Glob(v.into())
            } else if let Some(v) = source.strip_prefix("lines ") {
                Values::Lines(v.into())
            } else if let Some(v) = source.strip_prefix("words ") {
                Values::Words(v.into())
            } else {
                return Err(script_error(
                    *line,
                    "for source must be glob, lines, or words",
                ));
            };
            let (body, end) = parse_block(lines, position, true)?;
            require_end(*line, end)?;
            StatementKind::For {
                name: name.into(),
                values,
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

fn parse_statement(line: usize, text: &str) -> Result<Statement> {
    let kind = if let Some(rest) = text.strip_prefix("let ") {
        assignment(rest, false)?
    } else if let Some(rest) = text.strip_prefix("secret ") {
        assignment(rest, true)?
    } else if let Some(rest) = text.strip_prefix("capture ") {
        let (name, command) = split_once(rest, "<-")?;
        valid_name(name)?;
        StatementKind::Capture {
            name: name.into(),
            command: command.into(),
        }
    } else if let Some(command) = text.strip_prefix("$ ") {
        StatementKind::Run(command.into())
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
        let (path, value) = split_once(rest, "<-")?;
        StatementKind::Write {
            path: path.into(),
            value: value.into(),
            append: false,
        }
    } else if let Some(rest) = text.strip_prefix("append ") {
        let (path, value) = split_once(rest, "<-")?;
        StatementKind::Write {
            path: path.into(),
            value: value.into(),
            append: true,
        }
    } else if let Some(rest) = text.strip_prefix("copy ") {
        let (from, to) = split_once(rest, "->")?;
        StatementKind::Copy {
            from: from.into(),
            to: to.into(),
        }
    } else if let Some(v) = text.strip_prefix("remove ") {
        StatementKind::Remove(v.into())
    } else if let Some(v) = text.strip_prefix("print ") {
        StatementKind::Print(v.into())
    } else if let Some(rest) = text.strip_prefix("call ") {
        let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
        StatementKind::Call {
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
    let (name, value) = split_once(rest, "=")?;
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
fn split_once<'a>(value: &'a str, delimiter: &str) -> Result<(&'a str, &'a str)> {
    value
        .split_once(delimiter)
        .map(|(a, b)| (a.trim(), b.trim()))
        .ok_or_else(|| Error::message(format!("expected `{delimiter}`")))
}
fn valid_name(name: &str) -> Result<()> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(Error::message(format!("invalid name `{name}`")))
    }
}

fn extract_redirect(line: &str) -> Result<(&str, Option<(RedirectKind, &str)>)> {
    let mut quote = None;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == '\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            if quote == Some(c) {
                quote = None
            } else if quote.is_none() {
                quote = Some(c)
            };
            i += 1;
            continue;
        }
        if quote.is_none() {
            let (kind, len) = if line[i..].starts_with(">>") {
                (Some(RedirectKind::Append), 2)
            } else if line[i..].starts_with("2>") {
                (Some(RedirectKind::Stderr), 2)
            } else if c == '>' {
                (Some(RedirectKind::Stdout), 1)
            } else {
                (None, 1)
            };
            if let Some(kind) = kind {
                if found.is_some() {
                    return Err(Error::message(
                        "only one redirection is supported per command",
                    ));
                }
                found = Some((i, kind, len));
                break;
            }
            i += len
        } else {
            i += 1
        }
    }
    if let Some((index, kind, len)) = found {
        let path = line[index + len..].trim();
        if path.is_empty() {
            return Err(Error::message("redirection needs a path"));
        }
        Ok((line[..index].trim(), Some((kind, path))))
    } else {
        Ok((line, None))
    }
}
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '\'' || c == '"' {
            if quote == Some(c) {
                quote = None
            } else if quote.is_none() {
                quote = Some(c)
            }
        } else if c == '#' && quote.is_none() {
            return &line[..i];
        }
    }
    line
}
fn split_pipeline(line: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true
        } else if c == '\'' || c == '"' {
            if quote == Some(c) {
                quote = None
            } else if quote.is_none() {
                quote = Some(c)
            }
        } else if c == '|' && quote.is_none() {
            result.push(line[start..i].trim());
            start = i + 1
        }
    }
    if quote.is_some() {
        return Err(Error::message("unclosed quote"));
    }
    result.push(line[start..].trim());
    Ok(result)
}
fn words(source: &str, variables: &HashMap<String, String>) -> Result<Vec<String>> {
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
                let value = variables
                    .get(&name)
                    .cloned()
                    .or_else(|| std::env::var(&name).ok())
                    .ok_or_else(|| Error::message(format!("undefined variable `{name}`")))?;
                word.push_str(&value)
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
