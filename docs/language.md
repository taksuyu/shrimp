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

Values use the small typed model described below. `capture` produces a UTF-8 string and removes trailing CR/LF characters.
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
`2> path` replaces stderr; the non-redirected stream is still emitted. `> discard` or
`2> discard` drops the selected stream. One output redirection is accepted per command.

`< path` feeds raw file bytes to the first process and `<<< VALUE` feeds an expanded
scalar's UTF-8 bytes. Input can be combined with a following output redirect. It is
valid on ordinary commands, capture, retry, and timeout statements. No shell parsing,
word splitting, command substitution, or implicit encoding conversion is involved.
An early child exit may close stdin before all input is written; that ordinary broken
pipe is ignored and command status remains authoritative. Any other stdin write error
fails the command or timed pipeline instead of silently accepting truncated input.

Per-invocation environment overrides use an explicit prefix:

```shrimp
env PROFILE="release" COLOR="never" $ cargo build
```

Names are validated, values remain individual scalar values, overrides apply to every
process in the pipeline, and they do not mutate later commands or the Shrimp runtime.

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
It creates missing parent directories. `remove` fails for absent files.
`remove --recursive --force` removes a file or directory tree and succeeds when the
path is already absent, making it suitable for clean workspace builds.

Temporary paths and integer metadata are explicit:

```shrimp
temp_file response
temp_dir staging
file_size bytes <- "artifact.tar.gz"
modified_time changed_at <- "artifact.tar.gz"
```

Temporary statements bind an absolute string path and create it immediately. All
temporary paths are removed at workflow completion, including error completion;
parallel branches share the cleanup registry. Dry-run assigns a planned path but does
not create it. On Unix, managed files use mode `0600` and directories use `0700`.
`file_size` returns bytes as an integer. `modified_time` returns whole seconds since
the Unix epoch as an integer and rejects pre-epoch or overflowing values.

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
typed, and arity-checked; variable bindings are restored after a call. Filesystem and
process effects remain visible. A function implicitly returns its final statement's
value. `value VALUE` is a side-effect-free value statement, while assignments, record
construction, captures, metadata queries, temporary creation, and nested captured calls
also produce values. Effect-only final statements produce `missing`.

```shrimp
fn artifact_path name
  value "target/${name}.tar.gz"
end

call artifact <- artifact_path "release"
call artifact_path "debug" # discard the result
```

There is no early-return statement. `call NAME <- FUNCTION ARG...` preserves lists and
records rather than serializing them. Parameters are local, while caller bindings are
restored even when a call fails. The recursion depth limit remains 64.

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
shrimp [--check] [--dry-run] [--trace] workflow.shrimp [NAME=VALUE ...]
```

`NAME=VALUE` entries initialize both workflow variables and child environment values.
`--trace` writes expanded actions to stderr before execution. `--dry-run` implies
tracing and suppresses process and filesystem effects; captures become empty strings
and conditions choose their success branch so the plan can continue.

## Exit and error behavior

Shrimp exits nonzero on parse errors, missing tools, failed commands, timeouts, file
errors, bad function arity, or failed parallel branches. Diagnostics include the
logical source line. A successful run exits zero and reports command/file counts.

## Typed values and conditions (experimental)

Values are strings, booleans, signed 64-bit integers, lists, records, or an internal
missing value. Quoted literals and captures are UTF-8 strings; bare `true`/`false` and
integer assignment literals are typed. `glob`, `lines`, and `words` create lists when
assigned. Iterate with `for value in ${values}` or select with `${values[0]}`. Records
continue to use `${record.field}`. Lists and records cannot be interpolated whole;
select or iterate them. Scalar booleans and integers use canonical text spelling.

An `if` containing a comparison or boolean operator, or beginning with `exists`, is a
workflow expression. Equality is typed; ordering requires integers. `and`, `or`, `not`,
and boolean variables are supported. `exists` resolves against the current workflow
directory. There is no arithmetic, grouping, or general-purpose expression evaluation.
Other conditions remain direct commands.

Writes, appends, copy destinations, and redirection destinations create missing parent
directories. `--check` parses only and performs no effects. Workflow commands accept
the explicit stdin and environment forms documented above, while Rust callers can feed
bytes with `Pipeline::stdin`. Invalid UTF-8 capture is an error, while uncaptured output
bytes are forwarded without decoding.

Typed behavior is experimental. Function defaults, same-path parallel-write detection,
modules, arithmetic, command substitution, and shell compatibility remain deliberate
limitations or future ideas.

## Includes

```shrimp
include "lib/files.shrimp"
call copy_if_present "input" "output"
```

`include VALUE` expands exactly one scalar path, reads UTF-8 Shrimp source, parses it,
and executes its top-level statements in the current runtime. Functions and bindings
therefore become available after the include; process and filesystem effects are not
hidden. A path is resolved relative to the source file containing that include, not
the process launch directory or a prior `cd` statement. Nested includes follow the
same rule. Canonical files load once per workflow, cycles fail explicitly, and an
included parse/runtime error names the included file and retains line diagnostics.
Functions retain their defining source directory, so an include deferred inside a
function follows the same relative-path rule. Parallel runtimes share include state:
top-level effects execute once, while exported bindings and functions are copied into
each branch that requests the already-loaded file.

This is source composition, not a module/package system: it provides no namespace,
export list, implicit search path, remote source, version selection, or isolation.

## Potential future work

These are design notes, not implemented syntax or commitments:

| Idea | Possible shape | Difficulty | Main design issue |
| --- | --- | --- | --- |
| Command result records | `capture result <- unchecked $ tool`, then `result.success`, `result.code`, `result.stdout`, and `result.stderr` | Medium | Preserve the distinction between failure, status, raw bytes, and UTF-8 conversion without adding a broad byte type. |
| Default function parameters | `fn package source destination="dist"` | Medium | Requires quote-aware signatures, trailing-default validation, and a firm rule for typed defaults; keyword arguments would remain out of scope. |
| Parallel write collision checks | Reject two active branches claiming the same normalized destination | Medium–high | A shared registry can catch lexical matches, but symlinks and destinations that do not exist make perfect identity impractical. Dry-run must use the same checks. |
| Recursive include checking | Have `--check` follow statically resolvable include files | Medium | Interpolated paths cannot always be resolved without evaluating bindings; literal paths can be checked safely without running effects. |
| Namespaced modules | `import "release.shrimp" as release`, then `call release.publish ...` | High | Requires exports, qualified symbol tables, initialization and isolation rules, duplicate identity handling, and compatibility policy. `include` should remain sufficient until real workflows demonstrate this need. |
| Windows process-tree cancellation | Put timed children in a Windows Job Object | High | This is platform process-management work and needs Windows integration coverage; direct-child termination is not production-grade tree management. |

Other explicit non-goals remain a shell-compatible parser, implicit shell execution,
arithmetic/general expressions, package management, dependency graphs, caching, remote
execution, and plugins.
