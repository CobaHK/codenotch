# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Codenotch is a macOS app (Swift/SwiftUI) that pins a small notch to a screen
edge showing how much of each coding assistant's usage limit is burned, and
whether it's working, done, or waiting on input. `windows/` is a from-scratch
Windows port (Rust + Tauri 2 / WebView2) — same design and provider set, no
shared code with the Swift app. Treat the two as separate codebases with
separate build/test workflows.

## Building and testing — macOS (`Sources/`, `Tests/`)

```sh
brew install xcodegen   # once — project.yml generates the .xcodeproj
make build               # Debug build, ad-hoc signed, no Apple account needed
make test                # unit tests (generates project first)
make run                 # build, kill running instance, relaunch
make test-ci             # what CI runs: forces unsigned build
make verify-deps         # regenerates the project and diffs Package.resolved for drift
```

`make gen` (implicit dependency of the above) runs `xcodegen generate` from
`project.yml` — **never edit `Codenotch.xcodeproj` directly**, it's
regenerated and not the source of truth. `CODENOTCH_DEMO=1` runs the app with
fixed sample data instead of live readings.

To run a single test, use `xcodebuild test` directly with `-only-testing`, or
open the generated project in Xcode. `Tests/` mirrors `Sources/` by concern,
not necessarily by file — look for the existing test class closest to what
you're changing before adding a new one.

Signing is auto-detected (see comments in `Makefile`): Developer ID if present
(maintainer only), else a personal "Apple Development" cert, else ad-hoc.
`make release` (archive, notarize, Sparkle feed) needs a Developer ID
certificate and is maintainer-only — see `CONTRIBUTING.md`.

`Scripts/sign-local.sh` re-signs a built `/Applications/Codenotch.app` with a
stable self-signed identity so macOS keychain "Always Allow" grants survive
rebuilds during local development (ad-hoc builds have no stable code identity,
so the prompt otherwise returns on every launch).

Logs go to the unified log, not a window:
```sh
/usr/bin/log stream --predicate 'subsystem == "com.vinz.codenotch"' --level debug
```

## Building and testing — Windows (`windows/`)

All commands run from the `windows/` directory. CI only triggers on changes
under `windows/**`.

```powershell
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked
node scripts/check-ui-scripts.mjs        # the HTML/JS tray & settings pages aren't compiled by cargo
node scripts/test-claude-auth-ui.cjs
node --test test-codex-headline.cjs
```

Build the NSIS installer the way the release workflow does:
```powershell
cargo build --release --locked -p codenotch-hook --target-dir target/hook
cd codenotch
npx @tauri-apps/cli@2 build --config tauri.bundle.conf.json
```

`codenotch/` is the app (pill, hover card, settings, providers);
`codenotch-hook/` is a tiny helper Claude Code calls to report session events.
`codenotch.exe doctor` self-diagnoses credentials, data sources, icons, and
hooks. This tree is developed upstream at `Im-Midi/codenotch-windows` and kept
in sync here — see `windows/README.md` for the full provider/feature writeup.

## Architecture (macOS)

**Providers.** Every usage source implements `UsageProvider`
(`Sources/Providers/`), one file per vendor/mechanism, and declares its own
`Fidelity` — `.official`, `.derived`, or `.manual` — so the UI never presents
a guess as something a vendor published. Every failure path degrades to a
`ProviderStatus` (`stale`, `needsAuth`, `accessDenied`, `error`, …) rather
than an invented percentage — see `UsageProviderError`/`ProviderStatus` and
`CONTRIBUTING.md`'s "Adding a provider" section before writing a new one.
Credentials read from the keychain go through `CredentialCache` rather than
being re-read on every poll.

**Polling and state.** `UsageStore` (`Sources/Model/`) polls providers on a
timer, persists the last good reading across launches, and owns the
degrade-to-visible-status behavior. `Sources/Sessions/` is the parallel
concern of *liveness* — per-agent monitors (`ClaudeSessionMonitor`,
`CodexActivityMonitor`, etc.) that watch transcripts/processes to know if a
session is busy or waiting on the user, independent of usage numbers.

**Notch geometry.** The notch works in one-dimensional **stack space**
(`along`/`across`) regardless of which physical screen edge it's pinned to;
`NotchPlacement` (`Sources/Notch/`) is the only place that maps stack space
back onto real screen coordinates. `NotchLayout` holds every measurement,
each one quoted from `docs/design/frame-124-hover-tooltip.png` via
`Design.px(_:)` so layout can be checked directly against the design frame —
any change to `Sources/Notch/NotchLayout.swift` should be verified against
that image.

**Other areas:** `Sources/Features/` (ring/tooltip/pace SwiftUI views),
`Sources/Settings/` (preferences UI + `Preferences` storage), `Sources/PhoneLink/`
(local-network-only pairing/relay to the companion phone app — protocol in
`docs/phone-link-protocol.md`), `Sources/DesignSystem/` (palette/typography
tokens), `Sources/Vendor/zstd` (vendored decode-only Zstandard, BSD-3-Clause,
needed because Claude Desktop's HTTP cache is `content-encoding: zstd` and
macOS ships no decoder).

Design spec: `docs/specs/2026-08-28-usage-notch-design.md`. Implementation
history/decisions: `TASKS.md`. Provider-specific research lives under
`docs/plans/`.

## Architecture (Windows)

Modular by concern under `codenotch/src/`: one file per provider
(`claude_auth.rs`, `codex.rs`, `cursor.rs`, `grok.rs`, `antigravity.rs`,
`glm.rs`, …), plus `state.rs`/`config.rs` (persisted state and
`%APPDATA%\codenotch\config.json`), `tray.rs`/`traymenu.rs`/`trayicon.rs`
(system tray), `notchmenu.rs`/`dropzones.rs`/`focus.rs` (pill window and
edge-snapping), `settings_window.rs` (the settings WebView), `watcher.rs`
(session/activity detection), `hooks_install.rs` (wiring up Claude Code
hooks), `updater.rs` (minisign-verified self-update via a `latest.json`
feed), and `doctor.rs` (the `doctor` subcommand). UI surfaces
(`codenotch/ui/notch.html`, `codenotch/ui/settings.html`) are plain HTML/JS,
not compiled — hence the separate `check-ui-scripts.mjs` parse check in CI.
Three surfaces (tray, hover card, settings window) each keep their own
translation table; see the Translations section of `windows/README.md`
before adding user-facing strings.

## Conventions

- **Comments explain why, not what** — a hidden constraint, a workaround for
  a specific bug, a design decision that would look arbitrary otherwise. If
  removing a comment wouldn't confuse the next reader, don't add it.
- **No premature abstraction.** Three similar lines beat an early helper.
- **User-visible strings (macOS)** go through `L10n.t("English source")` —
  the English string *is* the key. Optional translations live in
  `Sources/Localizable.xcstrings`; a missing translation falls back to
  English and must never fail tests/CI. Don't cache `L10n.t` in a `static
  let` — lookup must see the current language. Windows' `i18n.rs` is a
  separate table; don't try to merge the two, but do reuse the exact English
  wording from `Sources/Localizable.xcstrings` when a string exists on both
  platforms.
- A provider adapter's job is to **never invent a number**: map every
  failure to an honest status the UI can render.
