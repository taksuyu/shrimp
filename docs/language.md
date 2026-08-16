# Shrimp language reference (prototype 0.1)

This document records the behavior implemented by the prototype. It is a reference,
not yet a backwards-compatibility promise.

## Execution model

A workflow is UTF-8 text evaluated from top to bottom. Relative paths and child
working directories start at the directory containing the workflow. A statement
failure stops its sequential block. Commands execute directly—Shrimp never passes a
command string to a platform shell—and pipelines use `pipefail` semantics.

Blank lines are ignored. `#` begins a comment outside quotes. A final backslash joins
the next physical line. Single quotes are literal; double quotes support `${name}` and
the escapes `\n`, `\t`, `\\`, and escaped quotes. Expansions always remain one argument.

## Values

```shrimp
let name = "ordinary value"
secret token = "${TOKEN_FROM_ENV}"
capture revision <- git rev-parse --short HEAD
```

Values are currently UTF-8 strings. `capture` removes trailing CR/LF characters.
Undefined variables are errors. Secrets behave like ordinary values except that their
contents are replaced with `[REDACTED]` in trace output.

Tab-separated input can be named as a record. Field count and field names are
validated, and fields are accessed with dotted interpolation:

```shrimp
record document tsv "${line}" fields name state path
print "${document.name}: ${document.path}"
```

## Processes

```shrimp
$ program arg "argument with spaces"
$ producer | transform | consumer
retry 4 $ unreliable-command
timeout 500ms $ possibly-stuck-command
if predicate-command
  $ success-command
else
  $ failure-command
end
```

For the command-line `retry` statement, the number is the total number of attempts,
not a count of additional retries. Shrimp waits a fixed 100ms between failed attempts.
This convention is specific to the workflow language statement; the Rust embedding
API's `Task::retry` method has its own documented argument semantics.

Durations accept `ms`, `s`, or `m`. A timeout kills every direct child; on Unix each
pipeline child has an isolated process group so descendants are killed too. Ordinary
commands remain in the terminal's foreground process group and therefore receive
interactive signals. Windows descendant cleanup will require job objects.

The last pipeline process's stdout/stderr is captured internally. With no redirection
both streams are emitted. `> path` replaces stdout, `>> path` appends stdout, and
`2> path` replaces stderr; the non-redirected stream is still emitted. One redirection
is currently accepted per command statement. Redirection is not accepted on `capture`.

## Files and directories

```shrimp
cd "subdirectory"
mkdir "output"
write "output/value" <- "complete contents\n"
append "output/log" <- "one more line\n"
copy "source" -> "destination"
remove "obsolete"
remove --recursive --force "generated-tree"
```

`write` creates a same-directory temporary file and renames it over the destination.
It does not create parent directories. `remove` fails for absent files.
`remove --recursive --force` removes a file or directory tree and succeeds when the
path is already absent, making it suitable for clean workspace builds.

## Matching

`match` selects a branch by string value. An optional `else` handles values without a
matching case:

```shrimp
match "${document.state}"
case published
  print "ready"
case draft
  print "not ready"
else
  $ false
end
```

## Iteration

```shrimp
for file in glob "src/**/*.rs"
  print "${file}"
end

for line in lines "${captured_text}"
  print "${line}"
end

for item in words "one two three"
  print "${item}"
end
```

Glob results are sorted and are relative to the current workflow directory where
possible. The loop variable is removed after the loop.

## Functions

```shrimp
fn package source destination
  copy "${source}" -> "${destination}"
end

call package "asset.txt" "dist/asset.txt"
```

Functions are available after their definition executes. Arguments are positional,
arity is checked, and variable bindings are restored after a call. Filesystem and
process effects remain visible.

## Parallel blocks

```shrimp
parallel
  $ cargo test
  $ cargo clippy
  call build_docs
end
```

Every direct child statement is a branch and all branches start before Shrimp waits
for them. Branches receive cloned variables, functions, context, and secret metadata;
variable changes do not merge. Reports and external effects do merge. Concurrently
writing the same path is intentionally the workflow author's responsibility.

A loop can also run with an explicit concurrency bound:

```shrimp
parallel for file in glob "src/**/*.rs" limit 4
  call check "${file}"
end
```

At most `limit` iterations run concurrently. Each iteration receives isolated
variables, just like a branch in `parallel`, and Shrimp waits for every iteration in a
started batch before reporting an error. A parallel construct nested inside another
parallel construct executes its branches sequentially. This nesting boundary prevents
inner loops or blocks from multiplying the concurrency selected by the outer workflow.

## Runner

```console
shrimp [--dry-run] [--trace] workflow.shrimp [NAME=VALUE ...]
```

`NAME=VALUE` entries initialize both workflow variables and child environment values.
`--trace` writes expanded actions to stderr before execution. `--dry-run` implies
tracing and suppresses process and filesystem effects; captures become empty strings
and conditions choose their success branch so the plan can continue.

## Exit and error behavior

Shrimp exits nonzero on parse errors, missing tools, failed commands, timeouts, file
errors, bad function arity, or failed parallel branches. Diagnostics include the
logical source line. A successful run exits zero and reports command/file counts.
