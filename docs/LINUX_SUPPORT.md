# Linux development and validation

Linux is an active Yield target. It does not wait for macOS release readiness.
Shared feature implementation and Linux qualification proceed on Linux;
macOS/AppKit/CoreAudio/APFS and Windows/MSVC/NTFS requirements are tracked in
separate platform-tagged Pulse issues. The original backlog and safety contracts
remain authoritative; a successful headless build is not full desktop acceptance.

The initial build baseline is Debian 13 x86_64 with the repository's pinned
Rust 1.93.0 and `x86_64-unknown-linux-gnu` target. Ubuntu and other distributions
need their own recorded qualification. The active frontend is GPUI; archived
Radiant/winit/vello instructions are not Linux app validation.

Install the development dependencies on Debian/Ubuntu using an administrator:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends build-essential pkg-config clang \
  libasound2-dev libfontconfig-dev libfreetype-dev libxcb1-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  mesa-vulkan-drivers xdg-utils tini
```

Normal application builds enable both GPUI X11 and Wayland backends independently
of `test-support`. A desktop session needs a working GPU/Vulkan driver, fonts,
ALSA-compatible output (including the desktop's PipeWire/PulseAudio integration),
and a desktop file chooser portal (`xdg-desktop-portal` plus the session's backend).
Do not override the real desktop's audio configuration with the headless fixture.

Run the local product lane from a clean committed checkout:

```sh
bash scripts/gpui-linux-ci.sh check
bash scripts/gpui-linux-ci.sh full
cargo run -- /path/to/disposable-wav-fixture
```

`check` verifies formatting and the locked normal application graph. `full` also
checks every workspace target and runs `cargo test --workspace --locked` with
one test thread, preserving owner-fixture deadlines. It uses `tini -s` when
available to reap deliberately orphaned fixture processes in containers whose
PID 1 does not reap. Normal desktop init services already perform that role.
The lane does not park or reset production state and does not invoke macOS
state-maintenance or hardware-only selectors. Preserve failed transcripts and
record the exact SHA, host, toolchain, commands and actual test counts in the PR.
Hosted CI remains disabled.

For explicitly headless test hosts, the opt-in helper
`scripts/internal/setup_headless_audio.sh` supplies a dummy ALSA device. Such
results cover test execution only. Actual X11/Wayland window creation, portal
dialogs, source registration/reopen, filesystem refresh, physical audio routing,
device loss/resume, keyboard/IME, clipboard/drag and packaging remain product
acceptance requirements. Unsupported ownership/reserve or publication paths
must continue to fail visibly and safely until a real Linux backend is qualified.
Do not convert fixture constructors or portable parser checks into physical
filesystem or native desktop claims.

Linux foundation: [Pulse #2065](https://pulse.iocore.dev/#/issues/2065).
Native runtime: [#2092](https://pulse.iocore.dev/#/issues/2092).
Clipboard/drag: [#2089](https://pulse.iocore.dev/#/issues/2089).
Filesystem/desktop integration: [#2086](https://pulse.iocore.dev/#/issues/2086).
Packaging: [#2083](https://pulse.iocore.dev/#/issues/2083).
