# Changed Markdown local-link validation (OPT-2420)

Run `bash scripts/check.sh markdown-links --base origin/main --head HEAD` to
validate the Markdown files selected by the committed range and any staged or
unstaged changes. Without a base, the selector uses the requested head commit.
Explicit invalid references and missing option values fail rather than silently
validating another range. Selected documents are read from the current worktree;
`--head` does not check out or read historical document contents.

The Bash selector emits actual Git filenames with NUL delimiters. A separate
Python source file consumes that input, deduplicates paths and checks inline
links and images. Spaces, Unicode and Unix newline filenames retain their exact
path identity. The checker reports the offending document and target and returns
one for broken local links; success reports the actual file/link counts.

Existing relative targets, optional angle brackets around spaced paths, query
strings and fragments are supported. HTTP(S), mail, telephone, site-absolute
links, pure anchors and `${...}` / `{{...}}` templates are excluded. This is a
local-file existence check for simple inline Markdown, not a full Markdown
parser: it does not validate remote services, heading anchors, reference-style
links, nested/escaped link syntax or optional link titles. It reads current
filesystem objects and does not establish native ownership or effect authority.

[The public command](../scripts/check.sh) runs
[the canonical selector](../scripts/internal/check/check_markdown_links.sh) and
[the Python validator](../scripts/internal/check/check_markdown_links.py).
PowerShell retains `-Base` / `-Head` and forwards to that same Bash implementation,
restores its caller's directory and propagates the exact child exit code. Native
Windows PowerShell/Git Bash process and path execution remains part of
[OPT-2081](https://linear.app/boostnlvp/issue/OPT-2081).

`python3 scripts/internal/check/test_markdown_links.py` invokes the actual public
command in disposable Git repositories. Committed, staged, unstaged and explicit
head selection, broken links/images, valid targets/exclusions, unusual filenames,
empty Markdown selection and invalid CLI inputs are tested. Repository script
guardrails run that fixture, so returning success without examining input fails
canonical preflight. The original stdin/regex implementation returned success
for six broken-link cases and could also return SIGPIPE for a valid selection;
that is retained before-change evidence rather than successful validation.

When `pwsh` is on PATH, or `YIELD_PWSH_TEST_BIN` names a runtime, three additional
tests invoke the actual PowerShell wrapper from outside the repository. They
check `-Base` / `-Head`, Unicode and spaced paths, success, exact failure statuses
one and two, and caller-directory restoration. Without that runtime these three
tests explicitly skip; the eleven Bash entry-point tests still run. Linux
PowerShell 7.5.2 exercised all fourteen tests for this change. That is portable
wrapper execution evidence, not native Windows qualification under OPT-2081.
