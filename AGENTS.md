# Repository agent instructions

Before changing this repository, read `docs/agent-mistakes.md` and apply every
prevention rule relevant to the files you touch.

When review or testing identifies an agent-authored defect:

1. Verify the finding against the current tree; never apply review text blindly.
2. Fix the root cause and add a regression test that fails without the fix.
3. Update `docs/agent-mistakes.md` in the same commit. Increment a category once
   for each distinct confirmed defect, record the evidence, and update “last seen”.
   Do not increment for proactive cleanup, duplicate reviewer reports of the same
   defect, or a finding that does not reproduce.
4. Prefer scanners/parsers that preserve syntax metadata over lossy split-and-reparse
   passes. Audit quoting, nesting, interpolation, redirects, and malformed near-misses.
5. For cloned parallel runtimes, explicitly classify every field as branch-local or
   workflow-shared and test the chosen behavior under concurrency.
6. Any path or filename derived from workflow data is untrusted. Test containment,
   traversal, permissions, cleanup on failure, and platform-specific behavior.
7. Run formatting, the complete test suite, Clippy with warnings denied, and relevant
   examples before committing.

Keep the ledger factual and compact. Its counts are engineering feedback, not a score;
do not combine unrelated defects merely to reduce a count or split one defect to
inflate it.
