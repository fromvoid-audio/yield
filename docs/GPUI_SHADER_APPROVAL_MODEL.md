# Profile-owned shader approval model

The Linux-verifiable model portion of OPT-1943 adds bounded custom source and
approval commands to the existing ordered preference owner. Import keeps exact
UTF-8 bytes (up to 64 KiB) and clears approval/enabling. Approval must match the
current source fingerprint. Replace, revoke and clear cannot carry old authority
forward; rejected commands preserve the last-good preference state.

The receipt is SHA-256 over the fixed Yield approval domain, shader contract
version and exact source bytes. Whitespace changes require fresh approval. The
profile file remains trusted local state: this is neither a user signature nor a
shader sandbox. Parsing, a file name or a successful compile never grants approval.
The renderer-facing model getter returns source only when enabled with matching
approval. Future runtime consumers must additionally require settled owner
persistence and their renderer/window generation fences.

This integration also carries additive, validated shipped-effect selection and
strength preferences from the prepared model. It does not add controls, shader
compilation or execution. OPT-1942/1945/1947 and native acceptance OPT-1931 remain
open. Original combined PR #150 is preserved for the remaining integration.

Linux verification runs `cargo test --locked -p yield-session --lib preferences::tests`,
the normal locked app check and headless UI suite. The actual session-owned
approval save/reopen fixture requires macOS because session bootstrap requires
the qualified physical reserve provider. Linux reports Unsupported and retains
that production gate. Existing full session tests also encounter this platform
restriction; focused model success is not a full-session pass. Native owned
approval persistence and restart are tracked in OPT-1959 under OPT-1953.
