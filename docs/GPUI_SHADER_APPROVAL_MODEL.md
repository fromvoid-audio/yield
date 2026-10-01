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

The model now feeds the integrated source review, shipped-effect controls and
confirmed macOS runtime described in [the visual effect contract](GPUI_VISUAL_EFFECT_CONTRACT.md).
Linux source checks do not qualify native compilation, renderer execution or
acceptance; those remain in the Mac verification queue. Original combined PR #150
is preserved for its unrelated Finder and catalog changes.

Linux verification runs `cargo test --locked -p yield-session --lib preferences::tests`,
the normal locked app check and headless UI suite. The actual session-owned
approval save/reopen fixture requires macOS because session bootstrap requires
the qualified physical reserve provider. Linux reports Unsupported and retains
that production gate. Existing full session tests also encounter this platform
restriction; focused model success is not a full-session pass. Native owned
approval persistence and restart are tracked in OPT-1959 under OPT-1953.
