# Agent mistake ledger

This ledger records confirmed agent-authored defects found during review. Counts are
number of distinct occurrences, not number of reviewer comments. The initial evidence
set is [PR #3](https://github.com/taksuyu/shrimp/pull/3), reviewed 2026-08-17.

| ID | Category | Count | Last seen | Prevention rule |
| --- | --- | ---: | --- | --- |
| PARSE-001 | Lossy syntax handling discarded quote/token boundaries | 3 | PR #3 | Preserve raw token spans or quote metadata through classification and evaluation. Never join parsed tokens and parse them again. Test quoted typed literals, whitespace inside operands, and operator text inside command arguments. |
| STATE-001 | Deferred/parallel execution lost required shared or lexical context | 2 | PR #3 | Classify cloned runtime state as local vs shared. Attach lexical source directories to deferred function bodies and synchronize workflow-global include-once state. Test deferred calls and concurrent branches. |
| SAFE-001 | Filesystem data crossed a trust or privacy boundary without validation | 2 | PR #3 | Treat workflow-derived path components as untrusted and avoid using them as destinations without containment. Give managed temporary resources private permissions and test cleanup and modes. |
| PORT-001 | A cross-platform test assumed a Unix utility | 1 | PR #3 | Use a portable test helper or gate utility-dependent tests with the appropriate target configuration. |
| PARSE-002 | Malformed near-miss syntax was accepted as a different construct | 1 | PR #3 | For every new delimiter, test missing, doubled, truncated, quoted, and adjacent forms; reject unsupported forms at parse time. |
| DESIGN-001 | Equivalent filesystem behavior was implemented in multiple places | 1 | PR #3 | Centralize effect helpers so behavior and diagnostics cannot drift between API and interpreter paths. |
| VALUE-001 | One typed-value consumer bypassed structured lookup | 1 | PR #3 | Route all variable, record-field, and list-index resolution through the same lookup function; test nested paths in every consumer. |

## Occurrence evidence

### PARSE-001 — 3

1. Quoted comparison operands were stripped and then retyped, making `"4" == 4` true.
2. Raw expression classification treated operators inside quoted command arguments as
   workflow operators.
3. Boolean recursion joined token slices, losing a quoted multi-word operand.

### STATE-001 — 2

1. A function defined in an included file did not retain that file's directory for an
   `include` executed later from the function body.
2. Parallel runtime clones had independent include-once sets, allowing top-level include
   effects to execute more than once per workflow.

### SAFE-001 — 2

1. The typed-manifest example used an untrusted manifest name as an output path component.
2. Managed Unix temporary files/directories used ambient permissions instead of
   explicitly private modes.

### Single-occurrence categories

- PORT-001: the `Pipeline::stdin` test invoked `cat` without a Unix gate.
- PARSE-002: `<<` was interpreted as file redirection rather than rejected.
- DESIGN-001: parent-directory creation was duplicated across filesystem and script paths.
- VALUE-001: condition operands used direct map lookup instead of record/list-aware lookup.

## Updating this file

Increment a count only after reproducing or otherwise confirming a distinct defect. Add
one concise evidence bullet under the category, update its “last seen” reference, and
add a regression test. If a new defect does not fit an existing root cause, add a new
stable ID rather than weakening an existing category.
