# Agent mistake ledger

This ledger records confirmed agent-authored defects found during review. Counts are
number of distinct occurrences, not number of reviewer comments. The initial evidence
set is [PR #3](https://github.com/taksuyu/shrimp/pull/3), reviewed 2026-08-17.

| ID | Category | Count | Last seen | Prevention rule |
| --- | --- | ---: | --- | --- |
| PARSE-001 | Lossy syntax handling discarded quote/token boundaries | 5 | PR #3 follow-up, 2026-08-18 | Preserve raw token spans or quote metadata through classification and evaluation. Use the configurable quote-aware delimiter scanner instead of raw substring checks. Test quoted typed literals, ignored suffixes, whitespace inside operands, and operator text inside arguments. |
| STATE-001 | Deferred/parallel execution lost required shared or lexical context | 3 | PR #3, 2026-08-18 | Classify cloned runtime state as local vs shared. Model waits between parallel workers and test nested parallel/deferred execution for deadlocks as well as incorrect results. |
| SAFE-001 | Filesystem data crossed a trust or privacy boundary without validation | 2 | PR #3 | Treat workflow-derived path components as untrusted and avoid using them as destinations without containment. Give managed temporary resources private permissions and test cleanup and modes. |
| PORT-001 | A cross-platform test assumed a Unix utility | 1 | PR #3 | Use a portable test helper or gate utility-dependent tests with the appropriate target configuration. |
| PARSE-002 | Malformed near-miss syntax was accepted as a different construct | 1 | PR #3 | For every new delimiter, test missing, doubled, truncated, quoted, and adjacent forms; reject unsupported forms at parse time. |
| DESIGN-001 | Equivalent effect behavior was implemented in multiple places | 2 | PR #3, 2026-08-18 | Centralize effect helpers so behavior, diagnostics, and error propagation cannot drift between execution paths. Never detach a worker without collecting its result. |
| VALUE-001 | One typed-value consumer bypassed structured lookup | 1 | PR #3 | Route all variable, record-field, and list-index resolution through the same lookup function; test nested paths in every consumer. |
| DOC-001 | User-facing wording obscured an execution semantic | 1 | PR #3, 2026-08-18 | Prefer concrete observable language. Review documentation statements against tests and replace tautological descriptions with explicit behavior. |
| OPS-001 | Shell quoting altered text passed to a developer tool | 1 | PR #3 follow-up, 2026-08-18 | Pass Markdown bodies through a single-quoted heredoc or file; never place backticks or expansion syntax inside a double-quoted shell argument. |

## Occurrence evidence

### PARSE-001 — 5

1. Quoted comparison operands were stripped and then retyped, making `"4" == 4` true.
2. Raw expression classification treated operators inside quoted command arguments as
   workflow operators.
3. Boolean recursion joined token slices, losing a quoted multi-word operand.
4. Function-call assignment used raw `contains("<-")`, so a quoted `<-` argument was
   mistaken for an assignment delimiter.
5. The first generic scanner version continued parsing ignored text after `#`, so an
   apostrophe in a comment incorrectly became an unclosed quote.

### STATE-001 — 3

1. A function defined in an included file did not retain that file's directory for an
   `include` executed later from the function body.
2. Parallel runtime clones had independent include-once sets, allowing top-level include
   effects to execute more than once per workflow.
3. A global include owner waited for parallel workers while those workers waited for the
   owner's registry, deadlocking nested includes.

### DESIGN-001 — 2

1. Parent-directory creation was duplicated across filesystem and script paths.
2. Pipeline stdin writer code was duplicated across normal and timeout execution, and
   both detached copies silently discarded write failures.

### SAFE-001 — 2

1. The typed-manifest example used an untrusted manifest name as an output path component.
2. Managed Unix temporary files/directories used ambient permissions instead of
   explicitly private modes.

### Single-occurrence categories

- PORT-001: the `Pipeline::stdin` test invoked `cat` without a Unix gate.
- PARSE-002: `<<` was interpreted as file redirection rather than rejected.
- VALUE-001: condition operands used direct map lookup instead of record/list-aware lookup.
- DOC-001: include documentation said effects “remain effects” instead of stating that
  effects remain visible.
- OPS-001: a double-quoted `gh pr create --body` argument executed Markdown backticks as
  shell command substitution instead of passing the body literally.

## Updating this file

Increment a count only after reproducing or otherwise confirming a distinct defect. Add
one concise evidence bullet under the category, update its “last seen” reference, and
add a regression test. If a new defect does not fit an existing root cause, add a new
stable ID rather than weakening an existing category.
