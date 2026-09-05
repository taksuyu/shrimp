# Agent mistake ledger

This ledger records confirmed agent-authored defects found during review. Counts are
number of distinct occurrences, not number of reviewer comments. The initial evidence
set is [PR #3](https://github.com/taksuyu/shrimp/pull/3), reviewed 2026-08-17.

| ID | Category | Count | Last seen | Prevention rule |
| --- | --- | ---: | --- | --- |
| PARSE-001 | Lossy syntax handling discarded quote/token boundaries | 9 | PR #5, 2026-08-24 | Preserve raw token spans or quote metadata through classification and evaluation. Route all language delimiters through the shared scanner. Keep quote and escape semantics identical across scanning and tokenization, and test quoted, escaped, nested, adjacent, and malformed forms. |
| STATE-001 | Deferred/parallel execution lost required shared or lexical context | 6 | PR #6 follow-up, 2026-08-26 | Classify cloned runtime state as local vs shared. Preserve logical ancestry and explicit assignment events across clones/caches, model waits and joins, and test nested deferred execution for deadlocks and caller-independent results. |
| SAFE-001 | Data crossed a trust or privacy boundary without validation | 9 | PR #6 follow-up, 2026-08-26 | Treat workflow-derived paths and confidential inputs as untrusted. Preserve secret provenance across every binding path, prevent secrets from entering argv, redact structured leaves only when sufficiently distinctive, and test collisions, cleanup, over-redaction, and exposure boundaries. |
| PORT-001 | A cross-platform test assumed a Unix utility | 1 | PR #3 | Use a portable test helper or gate utility-dependent tests with the appropriate target configuration. |
| PARSE-002 | Malformed near-miss syntax was accepted as a different construct | 2 | PR #6 follow-up, 2026-08-26 | For every new delimiter, test missing, doubled, truncated, quoted, and adjacent forms; reject unsupported forms at parse time. |
| DESIGN-001 | Equivalent effect behavior was implemented in multiple places | 2 | PR #3, 2026-08-18 | Centralize effect helpers so behavior, diagnostics, and error propagation cannot drift between execution paths. Never detach a worker without collecting its result. |
| VALUE-001 | One typed-value consumer bypassed structured lookup | 1 | PR #3 | Route all variable, record-field, and list-index resolution through the same lookup function; test nested paths in every consumer. |
| DOC-001 | User-facing wording obscured an execution semantic | 2 | PR #5, 2026-08-24 | Prefer concrete observable language. Review general claims against documented exceptions and tests; describe boundaries rather than making broader guarantees. |
| OPS-001 | Shell quoting altered text passed to a developer tool | 1 | PR #3 follow-up, 2026-08-18 | Pass Markdown bodies through a single-quoted heredoc or file; never place backticks or expansion syntax inside a double-quoted shell argument. |
| DRY-001 | Dry-run performed an effect-dependent filesystem read | 1 | PR #5, 2026-08-24 | Trace before effects and audit each effect-dependent statement for a dry-run value that does not access state suppressed by the plan. |
| EDIT-001 | Mechanical documentation edits damaged code or documentation validity | 6 | PR #6 follow-up, 2026-08-26 | Inspect the complete edited region after automated documentation changes, then compile and format before publishing; each documented item must retain exactly one declaration and all of its attributes; label illustrative snippets as text unless they are complete, tested doctests. |

## Occurrence evidence

### PARSE-001 — 9

1. Quoted comparison operands were stripped and then retyped, making `"4" == 4` true.
2. Raw expression classification treated operators inside quoted command arguments as
   workflow operators.
3. Boolean recursion joined token slices, losing a quoted multi-word operand.
4. Function-call assignment used raw `contains("<-")`, so a quoted `<-` argument was
   mistaken for an assignment delimiter.
5. The first generic scanner version continued parsing ignored text after `#`, so an
   apostrophe in a comment incorrectly became an unclosed quote.
6. The delimiter scanner treated backslashes as escapes inside single quotes even though
   the value tokenizer treats single-quoted backslashes literally.
7. Per-command environment bindings used raw `split_once('=')` rather than the shared
   quote-aware scanner.
8. Function calls used raw whitespace splitting for the function-name boundary.
9. Structured lookup used raw split/find passes for record and list delimiters.

### STATE-001 — 6

1. A function defined in an included file did not retain that file's directory for an
   `include` executed later from the function body.
2. Parallel runtime clones had independent include-once sets, allowing top-level include
   effects to execute more than once per workflow.
3. A global include owner waited for parallel workers while those workers waited for the
   owner's registry, deadlocking nested includes.
4. Include cycle detection tracked OS-thread waits but not inherited logical include
   lineage, so a parallel child could deadlock re-entering its owner's include.
5. Include exports inferred assignments from changed values, so cache contents depended
   on the first caller's pre-existing values.
6. Include exports inferred functions from new map keys, so cached redefinitions depended
   on which parallel caller loaded the file.

### DESIGN-001 — 2

1. Parent-directory creation was duplicated across filesystem and script paths.
2. Pipeline stdin writer code was duplicated across normal and timeout execution, and
   both detached copies silently discarded write failures.

### DOC-001 — 2

1. Include documentation said effects “remain effects” instead of stating that effects
   remain visible.
2. Redaction documentation claimed all secret contents were replaced immediately before
   documenting typed exceptions.

### SAFE-001 — 9

1. The typed-manifest example used an untrusted manifest name as an output path component.
2. Managed Unix temporary files/directories used ambient permissions instead of
   explicitly private modes.
3. `secret arg` accepted confidential values through argv, exposing them to shell
   history and local process inspection before trace redaction could apply.
4. Managed temporary paths used a predictable process-ID/counter name, allowing another
   local process to pre-create or anticipate workflow resources.
5. Secret workflow values could still be interpolated into child argv after input-time
   redaction protections were added.
6. Redaction ignored list and record leaves because it only handled scalar secret values.
7. Boolean and short-integer secret leaves were used as global substring replacement
   keys, corrupting unrelated trace text and digit substrings inside larger numbers.
8. Secret provenance was dropped by aliases, function parameters, and captured function
   results, allowing copied secrets to enter child argv.
9. Cached include exports omitted secrecy when a secret name was already secret before load.

### Single-occurrence categories

- PORT-001: the `Pipeline::stdin` test invoked `cat` without a Unix gate.
- PARSE-002: `<<` was interpreted as file redirection rather than rejected.
- PARSE-002: `<<<<` was accepted as `<<<` followed by a literal value prefix.
- VALUE-001: condition operands used direct map lookup instead of record/list-aware lookup.
- OPS-001: a double-quoted `gh pr create --body` argument executed Markdown backticks as
  shell command substitution instead of passing the body literally.
- DRY-001: metadata statements read files that preceding dry-run effects intentionally
  did not create.
- EDIT-001: docstring edits duplicated the `Context::arguments` and CLI `run`
  declarations and removed both platform attributes from `create_private_dir`, leaving
  the source syntactically invalid on Unix and semantically wrong cross-platform. The same
  documentation pass also labeled incomplete illustrative snippets as Rust doctests, causing
  the complete test suite to fail.
- EDIT-001: an autofix updated only one of two invalid `Context::new()` documentation calls;
  the remaining example was still excluded from doctest validation.

## Updating this file

Increment a count only after reproducing or otherwise confirming a distinct defect. Add
one concise evidence bullet under the category, update its “last seen” reference, and
add a regression test. If a new defect does not fit an existing root cause, add a new
stable ID rather than weakening an existing category.
