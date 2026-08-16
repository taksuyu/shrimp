# Workspace document benchmark

This directory defines one deliberately non-trivial workflow three times: in Bash,
Nushell, and Shrimp. It is intended as a reproducible comparison, not as an example
carefully chosen to make one language win.

The input document, [`workspace.tsv`](workspace.tsv), is the workspace's source of
truth. Each row contains a document name, its state (`published` or `draft`), and a
source path. Every implementation must:

1. clean its output workspace so stale files cannot survive;
2. validate the state and source path in every row;
3. route documents to `published/` or `drafts/`;
4. use a reusable build function rather than duplicate the two flows;
5. calculate a SHA-256 digest and write a deterministic `index.tsv`; and
6. produce byte-for-byte identical workspaces when run repeatedly.

This combines control flow, structured input, repeatable patterns, external tools,
filesystem management, and build-system integration. The TSV is intentionally a
simple existing-document format that all three tools can consume without adding a
JSON parser dependency.

## Run it

The Makefile is the common harness:

```console
make compare          # run implementations available on this machine, then diff
make repeatability    # run each twice and verify that its output is unchanged
make bash             # run just one implementation
make nu
make shrimp
make clean
```

`make compare` requires Bash, Nushell, Cargo, and `sha256sum`. Individual targets are
useful when Nushell is not installed. Outputs are isolated below `.work/<tool>` so a
run cannot accidentally pass by observing another implementation's artifacts.

## What the comparison exposes today

| Concern | Bash | Nushell | Shrimp 0.1 |
| --- | --- | --- | --- |
| Document rows | `read` assigns fields | rows become lists | `record ... tsv` names validated fields |
| Reusable pattern | function and local variables | custom command and lexical values | `fn`, with call bindings restored afterward |
| Branching | `case` | structured `match` | string `match` plus exit-status `if` |
| Failure behavior | needs `set -euo pipefail` plus explicit validation | errors and typed pipeline values are native | commands and filesystem statements fail fast |
| Workspace paths | depend on caller unless the script normalizes them | depend on caller in this example | relative paths are anchored to the script automatically |
| Clean rebuild | external `rm -rf` | native `rm -rf` | native recursive, forceful `remove` |
| Index records | formatted text | structured values converted to text | formatted text |

Shrimp now handles the benchmark's TSV records, multi-way branch, recursive cleanup,
and bounded parallel document builds directly. Nushell remains strongest when a flow
needs general structured data transformations: Shrimp's records intentionally cover
named TSV fields rather than arbitrary nested values. Bash is compact and ubiquitous,
but its safety properties
come from conventions (`set -euo pipefail`, quoting, and careful `read` usage).

Shrimp's advantages here are workflow-specific: its working directory is stable,
expansion never causes word splitting, failures stop the workflow by default, and
function variables do not leak. The remaining external `sha256sum` and `cat` calls
are ordinary tool composition; richer collection values would make the deterministic
post-parallel index assembly native as well.
