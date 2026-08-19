# Shrimp

Shrimp is an experiment in replacing the part of Bash and Python nobody enjoys:
workflow glue. **You write a small `.shrimp` workflow, not a Rust program.** The
runner is a single native binary and invokes tools directly without a shell.

```shrimp
# ci.shrimp
let out = "target/package"
mkdir "${out}"

$ cargo test
$ cargo build --release

capture revision <- git rev-parse --short HEAD
write "${out}/build.txt" <- "revision=${revision}\n"

if git diff --quiet
  append "${out}/build.txt" <- "tree=clean\n"
else
  append "${out}/build.txt" <- "tree=dirty\n"
end

$ find src -name "*.rs" | sort | wc -l
```

```console
cargo run -- ci.shrimp
# Once installed: shrimp ci.shrimp
```

This is now an actual language prototype rather than merely a Rust API for launching
commands. The lower-level Rust library remains available as the execution engine.

## Language tour

### Commands and pipelines

Commands start with `$`. Arguments are passed directly to the executable, so values
do not undergo shell splitting, glob expansion, or accidental code execution. Pipes
connect real OS processes and a failed command fails the workflow.

```shrimp
$ printf "%s\n" "hello world" | tr a-z A-Z
```

Unlike Bash, `"${value}"` and `${value}` both remain one argument. There is no subtle
quoted-versus-unquoted expansion mode to remember.

### Values and captured output

```shrimp
let profile = "release"
capture revision <- git rev-parse --short HEAD
$ cargo build --profile "${profile}"
print "built ${revision}"
```

Captured output has trailing newlines removed. Variables supplied on the command line
are available to interpolation and child processes:

```console
shrimp deploy.shrimp ENV=staging VERSION=1.2.3
```

Undefined variables and malformed quotes are errors with source line numbers instead
of silently becoming empty strings.

### Workflow-oriented file operations

```shrimp
mkdir "target/package"
write "target/package/version" <- "${revision}\n"
append "target/package/log" <- "built ${revision}\n"
copy "assets/config.json" -> "target/package/config.json"
remove "target/package/obsolete.txt"
cd "subproject"
```

`write` uses a same-directory temporary file and rename, so readers never observe a
partially written result. Paths are relative to the script directory, not whichever
directory happened to launch Shrimp.

### Conditions and retries

Conditions use a process exit status—the universal composition protocol of command
line tools—without requiring `[ ... ]` punctuation or `$?` bookkeeping.

```shrimp
if git diff --quiet
  print "clean"
else
  print "dirty"
end

retry 3 $ curl --fail --silent "${health_url}"
timeout 30s $ ./integration-tests
```

Retries are fail-fast after the requested attempts. Timeouts terminate the pipeline
and, on Unix, its descendant process groups. Nested `if` blocks are supported.

### Loops, functions, and parallel work

```shrimp
fn check file
  $ rustfmt --check "${file}"
end

for file in glob "src/**/*.rs"
  call check "${file}"
end

parallel
  $ cargo test
  $ cargo clippy -- -D warnings
end
```

Loops accept `glob`, `lines`, or `words` sources. A parallel block runs each direct
child statement concurrently; variable mutations are isolated per branch, while file
and process side effects are shared.

### Redirection, traces, and secrets

```shrimp
$ cargo metadata > "target/metadata.json"
$ diagnostics 2> "target/diagnostics.log"
secret token = "${DEPLOY_TOKEN}"
$ deploy --token "${token}"
```

`>`, `>>`, and `2>` redirect final process output. `--trace` prints expanded actions;
values declared with `secret` are replaced by `[REDACTED]` in traces. `--dry-run`
prints the plan without launching commands or changing files.

### Comments and continuation

`#` starts a comment outside quotes. A trailing `\` continues a statement:

```shrimp
$ cargo build \
  --release
```

## Why this is materially different from writing commands in Rust

- The file is interpreted immediately; there is no edit/compile cycle for workflows.
- The syntax gives pipelines, capture, interpolation, conditions, retries, and atomic
  file changes first-class semantics rather than rebuilding them around `Command`.
- Commands remain ordinary installed utilities, so existing tools compose naturally.
- Errors contain the workflow line and subprocess status/stderr.
- Distribution requires one small runner, while an individual workflow is plain text.

The intent is not to hide command lines. It is to remove quoting traps, implicit global
state, unchecked failures, `$?`, temporary-file ceremonies, and repetitive Python
`subprocess` code while keeping the flexibility that makes shell workflows useful.

## Current proof-of-concept boundary

Implemented now:

- direct commands and streaming pipelines;
- output capture and variables;
- conditionals and retries;
- explicit working directories and child environments;
- atomic writes, append, copy, remove, and directory creation;
- comments, quoting, escapes, continuation, and line-aware diagnostics;
- glob/line/word iteration and reusable functions;
- parallel workflow branches;
- stdout/stderr redirection and cancellable command timeouts;
- secret-aware tracing and side-effect-free dry runs;
- TSV records with named field access and string `match` branches;
- recursive workspace removal and concurrency-bounded parallel loops;
- typed values, typed conditions, list indexing, and typed function results;
- explicit file/value stdin and per-command environment overrides;
- managed temporary paths and integer file metadata.

The initial language contract is documented in [`docs/language.md`](docs/language.md).
Remaining production-hardening work includes Windows job-object cancellation, parallel
write collision checks, file durability policy, and a compatibility/versioning policy
once the syntax has had real-world use.

## Rust embedding API

The engine also exposes `Cmd`, `Pipeline`, `Task<T>`, filesystem tasks, and `Context`
for applications that need to construct workflows programmatically. `Task<T>` offers
`map`, `and_then`, `tap`, `retry`, and `timeout`. This layer is implementation machinery
for the language rather than the primary user experience.

## Try the included workflow

```console
cargo run -- examples/ci.shrimp
cat target/shrimp-example/build.txt
```

For a more demanding side-by-side exercise, the
[`examples/workspace-comparison`](examples/workspace-comparison/README.md) benchmark
implements the same document-driven workspace build in Bash, Nushell, and Shrimp and
uses a Makefile to compare their outputs and repeatability.

## Typed workflow additions

Shrimp's deliberately small value model contains strings, booleans, signed integers,
lists, records, and an internal missing value. Bare `true`, `false`, and integer
assignments are typed. `glob`, `lines`, and `words` produce lists when assigned; lists
can be iterated (`for item in ${items}`) or indexed (`${items[0]}`). Records use named
access (`${document.path}`). A whole list or record cannot be interpolated, avoiding
an implicit joining or serialization convention.

Workflow conditions support typed `==` and `!=`, integer `<`, `<=`, `>`, and `>=`,
`and`, `or`, `not`, boolean variables, and `exists PATH`. Conditions without these
markers remain direct command-status checks. Shrimp intentionally has no arithmetic or
arbitrary expression evaluator.

Writes, appends, copies, and redirection destinations create missing parents. The
`--check` runner option parses without executing. Rust embedders may feed bytes directly
to the first pipeline process with `Pipeline::stdin`.

Still deliberately absent are function defaults, parallel same-path write detection,
namespaced modules, and shell compatibility. These typed additions remain experimental
and do not constitute a compatibility promise. The selected workflow additions stay small:

```shrimp
fn artifact_path name
  value "target/${name}.tar.gz"
end
call artifact <- artifact_path "release"

$ tool < "input.txt"
$ tool <<< "${captured}" > "output.txt"
env PROFILE="release" $ cargo build

temp_file scratch
temp_dir staging
file_size bytes <- "output.txt"
modified_time changed_at <- "output.txt"
```

Functions implicitly return their final statement value and accept typed arguments;
there is no early-return control flow. Temporary paths are cleaned up when the workflow
ends. Metadata values are integers. See the language reference for exact semantics and
for a difficulty-ranked list of potential future work.

## Reusable workflow files

`include` is a deliberately smaller feature than a module system:

```shrimp
include "lib/release.shrimp"
call publish "target/package.tar.gz"
```

An included UTF-8 file is parsed and executed directly, so its function definitions
and top-level bindings become available to the caller and its explicit effects remain
visible. Paths are relative to the file containing the `include`, nested includes work
the same way, and each canonical file is loaded once per workflow. Include cycles and
errors in included files name the relevant file. There are no namespaces, implicit
search paths, remote imports, package resolution, or compatibility/version metadata.
