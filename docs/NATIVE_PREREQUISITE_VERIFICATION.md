# Native source, profile and startup prerequisite verification

The active `native-prerequisites.yml` workflow executes the existing acceptance
harnesses for OPT-2368, OPT-2378 and OPT-2405 on real macOS/APFS and Windows/MSVC
runners with the repository-pinned Rust toolchain. The script records the exact
head, kernel/account/filesystem/SDK/resource facts, commands and complete results.
It refuses other hosts and zero-test results; fixed selectors require exact
passing counts without ignored tests. The profile suite's standalone process
helper remains intentionally ignored while its parents launch and reap real
native children.

| Case | Native hosts | Acceptance scope |
| --- | --- | --- |
| profile-generations | macOS, Windows/MSVC | Original acquisition UUID/serial, clone-held OS lock, substitution/exhaustion and child-process release |
| source-release-policy | macOS, Windows/MSVC | Two original Registered policy cases, generic missing-policy compatibility and two separate arithmetic cases |
| source-release-policy integration | macOS | Three original all-owner graph/missing-policy/foreign-family refusal cases |
| startup-production | macOS | Original production bootstrap and exact descriptor-bound historical rebuild |
| startup-control-failure | macOS | Original production BEGIN-denial diagnostic, preserved journal and retained credits |
| startup-namespace-failure | macOS | Original production namespace failure and reply/worker lifetime |
| sdk-and-startup-components | macOS | Strong/Weak/provider/refund/poison/proof/conflict regressions and canonical native application check |

Every lane additionally checks the library's normal and test targets on its
supported native compiler/SDK. Production startup cases use separate fresh runner
accounts: an intentionally unsettled intent or failed bootstrap can preserve a
dirty volume marker, and joined worker teardown does not authorize another profile
to adopt it. The workflow never redirects the native home, substitutes a registry,
erases a marker or turns a fixture into a physical grant.

These are component/native regression requirements. Historical startup rows and
arithmetic cases are not qualified issuance. Source policy tests must retain their
expected physical-family Unsupported refusal and original cleanup. Full Source
allocation policy, joint WAL decisions, attempts, capture/terminal effects,
publication/refund/cleanup/settlement and release application acceptance remain
separate. Actual native results are pending until every required lane succeeds.

The first run confirmed production startup on macOS and original Source policy
execution on Windows/MSVC. It exposed test defects now corrected: canonical
macOS temporary namespaces preserve no-follow checks; lock diagnostic reads use
the same held Windows file object and drop the duplicate before release checks;
recovery assertions borrow the original typed retained error. Startup control
faults deny only BEGIN inside the original funded reader, after real reserve
bootstrap transactions, without enabling the startup/provider component hook.
A real SQLite regression checks that unrelated original transactions remain
usable and the selected failure retains its backing through consumer destruction.
Production startup also compares every field of the original claim tuple.
