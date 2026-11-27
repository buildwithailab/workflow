# macOS development and acceptance

The Mac implementation uses the same repository and shared application code as Windows. Platform adapters provide PTY process ownership, Pasteboard access, WKWebView actions and profiles, preview processes, Dock badges, and application-bundle updates. Do not copy a Windows account or runtime profile to the Mac.

The current local candidate targets Apple Silicon and macOS 14 or later. The deployment target is a build setting, not proof that every supported OS version has passed acceptance. Intel and universal packages are not provided by this build flow. Real provider sign-in, Teams, desktop notifications, and supported OS versions must be verified on the intended machines before declaring a stable Mac release.

Claude quota lookup currently reads file-backed credentials; Keychain-only credentials can therefore show usage as unavailable even when the CLI is signed in. That must not be presented as a signed-out account. Validate independent account selection with real Mac logins before relying on multiple accounts; separate profile directories alone do not prove Keychain isolation.

## Local build

Use the Rust toolchain pinned in `rust-toolchain.toml`, Xcode Command Line Tools, Node.js for development scripts, and the Tauri 2 CLI. Keep development tools under ignored storage such as `target/devtools`, outside the application package. The build script expects `target/devtools/node_modules/.bin/tauri`; set `WORKFLOW_TAURI_CLI` to use another installed Tauri CLI.

From the repository root:

```sh
cargo test --locked
node --test tests/terminal-input.cjs tests/commit-security.cjs
sh scripts/build-macos.sh
node scripts/licenses.cjs --target aarch64-apple-darwin --output target/macos-third-party-notices.txt
python3 scripts/package-macos.py
```

The normal application is `target/release/bundle/macos/Workflow.app`. The local packaging script creates `dist/Workflow-Update-macOS-arm64.app.zip` and its SHA-256 file. It verifies bundle identity, version, Apple Silicon executable, embedded update verification key, code signature, dependency notices, resource allowlist, and absence of the desktop test bridge. It never publishes. The designated Windows release account then prepares the signed Mac update assets as described below; the Mac ZIP alone is not a complete update release.

The Mac configuration uses the rounded `icons/icon-macos.png` and Retina `icons/icon-macos@2x.png`. Before both Tauri builds and development runs, `scripts/icons-macos.sh` uses macOS tools to generate `target/macos-icons/Workflow-Mac.icns` at standard and Retina sizes. The explicit ICNS entry also prevents development mode from falling back to the shared Windows PNG. Finder, Dock and Launchpad all use this bundle icon; the Windows icon assets remain separate. Keep one installed `Workflow.app` in a stable Applications location.

Existing Launchpad bookmarks can follow older test copies even after those copies move outside application search locations and their Launch Services entries are unregistered. Verify that the visible Launchpad entry resolves to the current installed application and that its cached icon matches the installed bundle. Any machine-specific repair must have a private rollback backup, affect only Workflow entries, and preserve other applications' icons, folders and positions. Keep runtime caches, bookmarks and repair backups outside source and packages. A custom acceptance profile still requires its existing launcher; launching the bundle directly does not inherit the launcher's environment. Do not move account caches or embed local profile settings in the application to refresh its icon.

The build and package scripts create an **ad-hoc signed candidate**. That signature is not an Apple Developer ID signature and does not provide notarization. Until Developer ID signing is available, release notes must disclose this and give Apple's application-specific first-launch steps. Update-manifest signing is still required. A future Apple-signed build must use an authorized signing environment and validate notarization and Gatekeeper separately; never disable platform security checks to make acceptance pass.

## Native application tests

Build a separate debug-only application with `desktop-tests` explicitly enabled:

```sh
target/devtools/node_modules/.bin/tauri build --debug --bundles app --features custom-protocol,desktop-tests -- --locked
mkdir -p target/test-tools
rustc tests/provider-fixture.rs -o target/test-tools/provider-fixture
node tests/macos.cjs
node tests/macos-features.cjs
```

`WORKFLOW_MAC_TEST_EXE` may select another debug test application. The suite uses temporary project and provider profiles, controlled CLI fixtures, authenticated local MCP routing, real WKWebView input, and the real system Pasteboard. It preserves the original clipboard items and types and stops if they cannot be safely restored. Test data and captures must stay in ignored or temporary storage.

The test bridge requires both a debug build and the explicit feature, plus test mode and an isolated profile at runtime. A release build with `desktop-tests` must fail to compile. Never distribute the test application.

Controlled providers verify application mechanics; they do not establish that an installed, signed-in CLI works. Separately test Codex, Claude Code, and GitHub Copilot CLI with their own accounts, current model capabilities, conversation resumption, approval cards, custom question replies, and file/image attachments. Attaching a view, changing tabs, and reading history must not start providers or send input. Closing the app must stop only its owned processes; reopening must not automatically restart them.

`macos-features.cjs` exercises shared model-context caching, custom quota commands, measured session usage and settings, editable native question replies, the verification MCP boundary, and safe data maintenance with all three controlled providers. Workflow children receive only an explicit environment allowlist and generated homes. Test commands and journals are generated at runtime. Native cleanup checks include macOS path aliases and GitHub CLI configuration precedence, including redirected or not-yet-created account directories. Individual groups can be selected with `path`, `models`, `quota`, `usage`, `verification`, `questions`, `maintenance`, `github`, `network`, `deadlines`, `storage` or `memory`.

The browser suite launches the installed Google Chrome with its ordinary OS GUI home and a new private user-data directory, then connects Workflow to that fixture profile. The Workflow and provider processes retain synthetic homes. It does not copy an existing Chrome profile or change browser security flags. Both the real system clipboard contents and only the test-owned Chrome process are restored or stopped during cleanup.

Also verify Finder file and image drops, screenshot paste in both Terminal and Chat, Dock and popup behavior while minimized/backgrounded, and application close/quit paths. Teams requires a signed-in Mac Teams Web connection and an explicitly authorized destination and message. Do not send a real message merely to satisfy a fixture test.

## Startup error acceptance

Recoverable Mac setup errors are handled before they can reach Tauri's panic boundary. The error dialog is scheduled on the native main operation queue after the event callback returns. A failure after window creation uses a non-blocking sheet; an earlier failure uses a standalone alert. Dismissing the error exits with status 1. Partially initialized resources are stopped, and exit-time update installation is permitted only after the entire setup succeeds. Profile permission and ownership checks still reject unsafe directories without changing their contents or modes.

Run the assisted startup checks with a fresh run name. They create only synthetic profiles and app copies under ignored `target/startup-checks`. Child environments use an explicit system-variable allowlist, synthetic HOME and provider directories, and missing custom provider executables so fixed installation paths cannot reach real CLIs. Automatic updates are disabled in the synthetic profile. Normal builds omit `WORKFLOW_TEST_MODE` entirely. Observe each requested window, dismiss an expected error with **OK**, and quit healthy windows normally. The duplicate-instance check also asks you to verify and quit the first application.

```sh
WORKFLOW_MAC_STARTUP_RUN=debug-check WORKFLOW_MAC_STARTUP_BRIDGE=1 node tests/macos-startup.cjs permissions
WORKFLOW_MAC_STARTUP_RUN=debug-check WORKFLOW_MAC_STARTUP_BRIDGE=1 node tests/macos-startup.cjs corrupt
WORKFLOW_MAC_STARTUP_RUN=debug-check WORKFLOW_MAC_STARTUP_BRIDGE=1 node tests/macos-startup.cjs duplicate
WORKFLOW_MAC_STARTUP_RUN=debug-check WORKFLOW_MAC_STARTUP_BRIDGE=1 node tests/macos-startup.cjs late
WORKFLOW_MAC_STARTUP_RUN=debug-check WORKFLOW_MAC_STARTUP_BRIDGE=1 node tests/macos-startup.cjs normal
```

The debug checks require the explicitly enabled `desktop-tests` build above. The late case creates an ordinary I/O failure in the test bridge's control-file write after the main window and services have initialized. Its exit observer reads the updater's actual installation state; failed setup must leave that state untouched, whereas a healthy exit must enter the normal update check. The bridge and observer are absent from a normal release build.

Also repeat `permissions`, `corrupt`, `duplicate`, and `normal` against the normal app extracted from the final locally tested ZIP: set `WORKFLOW_MAC_STARTUP_APP` to that bundle, use a fresh `WORKFLOW_MAC_STARTUP_RUN`, and omit `WORKFLOW_MAC_STARTUP_BRIDGE`. No launch-state suppression arguments are used. Keep the installed user application, its account caches, and its project/conversation data outside these tests. Neither running these checks nor building a local candidate authorizes a commit or publication.

## Updates and Windows compatibility

Mac updates use `Workflow-Update-macOS-arm64.app.zip` and the signed `Workflow-Update-macOS-arm64.json`. They share the pinned public verification key with Windows but use a platform-specific package and manifest. Apple application signing and update-manifest signing are separate checks. A signature that authorizes a Windows asset cannot authorize a Mac installation.

The update signing key stays in the designated Windows release account's non-exportable key store. After reviewing the final Mac ZIP, that account can sign its manifest with `scripts/sign-update.ps1 -Platform macOS-arm64` and the script's required package/release arguments. Never export the private key or put it in a repository, package, or Mac profile.

Mac clients check `https://buildwithailab.github.io/workflow/updates/Workflow-Update-macOS-arm64.json`. Their package and version-specific manifest are fetched from the immutable `v<version>-macos` release. Windows keeps its existing `releases/latest/download/Workflow-Update-Windows-x64.json` address and `v<version>` package tags. Mac releases must always use `make_latest=false`; a regular Mac release does not need GitHub's prerelease flag.

After accepting the exact Mac ZIP, run on the designated Windows signing account:

```powershell
powershell -NoProfile -File scripts/prepare-macos-update.ps1 -Package <tested-mac-zip> -ReleaseNotes <public-user-facing-notes> -Output <new-output-directory>
```

This captures and validates the Mac package, signs its manifest with the existing non-exportable key, and prepares the ZIP, checksum and signed JSON. It also validates the exact manifest/package pair and creates a Pages payload. It does not publish, commit or push. Review all three public assets and the user-facing notes; publish them together from a draft `v<version>-macos` release with `make_latest=false`.

The `macOS stable update channel` workflow selects the highest immutable regular Mac release, verifies its manifest signature and package size/hash, and deploys only the Mac manifest through GitHub Pages. It refuses a downgrade or different package/notes for an already published version. Preview, Windows, draft and mutable releases are excluded. Enable GitHub Pages with **GitHub Actions** as its source before the first authorized deployment. No signing key is required in GitHub Actions; only public signed metadata is deployed. This Pages site is reserved for the Mac update channel.

Automatic updates default to enabled. The shared scheduler checks daily while Workflow is open, downloads a verified new version, displays the update notice, and installs it on normal application exit or **Update and restart**. Explicitly disabled settings remain disabled. Updating stops owned provider terminals and preserves project/account/conversation data; it must not automatically restart providers.

Already published Mac 0.1.8 packages use the former repository-wide latest address. They need one manual installation of the first package containing the separate Mac channel; later releases can update automatically. Do not change the locked Windows release or its latest selection to bootstrap an old Mac client. Until code and the signed channel are published together, a local test alone does not activate updates for public installations.

Test the full check/download/signature path with an isolated debug-only loopback transport and real package replacement/restart/failure recovery on disposable normal app copies. Production builds must contain neither the test transport nor the desktop test bridge. Verify both public Mac downloads and the Windows latest manifest after every authorized publication.

The Mac installer copies its verified running executable into a private staging directory as a finite helper. After the parent exits, the helper rechecks the old and new bundles and uses exclusive native renames, which cannot nest a bundle inside an existing destination. A failed replacement restores the previous bundle; an unsuccessful rollback retains the recovery copy. Receipts distinguish installation, restart and rollback failures. Downloaded archives and completed staging directories are cleaned up. An explicit custom data profile reopens with the same profile and a fixed allowlist of home-directory locations; credentials and arbitrary environment variables are never forwarded.

For native updater acceptance, prepare normal base and target packages with different versions in disposable source copies, leaving the shared source version unchanged. Have the designated Windows account validate the target ZIP and sign its unpublished test manifest; transfer no private key. Build the base debug test app with `custom-protocol,desktop-tests`. The loopback transport requires a debug build, explicit test mode, an isolated profile and a literal `127.0.0.1` origin with a port. It retains normal signature, size and hash verification. Normal release builds cannot enable that transport through environment variables.

Run `node tests/macos-update.cjs helper` with `WORKFLOW_MAC_UPDATE_BASE_APP` pointing to the normal base app and `WORKFLOW_MAC_UPDATE_ZIP` to the target ZIP. Run `node tests/macos-update.cjs flow` with the same ZIP, `WORKFLOW_MAC_UPDATE_TEST_APP` pointing to the debug test app, `WORKFLOW_MAC_UPDATE_MANIFEST` to its signed target manifest and `WORKFLOW_MAC_UPDATE_FOREIGN_FEED` to an unchanged public Windows manifest. Choose a fresh `WORKFLOW_MAC_UPDATE_RUN` name for every run. `WORKFLOW_MAC_UPDATE_CASES` can select named flow cases when investigating a failure.

The helper cases cover installation, changed resources, version mismatch, occupied backup, a real immutable-file rename failure and rollback, waiting for the parent, and native restart. Flow cases cover missing metadata, bad signature, a correctly signed foreign platform, changed download bytes, read-only installation, cancellation, automatic install on exit, and the real **Update and restart** button. Successful flows start all three controlled providers, verify that updating stops them, and preserve account markers, model/effort settings, conversation identifiers and history bytes. Runtime state and last-use time may change when an old process stops or a saved view opens; these are not conversation mutations. A disabled automatic setting must remain disabled after an explicit update.

Fixtures use private canonical temporary directories outside protected Documents folders, so LaunchServices does not need to inherit the developer terminal's document access. Only a private locator is written under ignored `target/update-checks`. The assisted native steps require observing the normal version, signed notes and closed saved sessions, recording `native-ui-verified` in that case's temporary directory, and quitting normally. Record confirmation only after actual UI inspection. `node tests/macos-update.cjs normal` additionally checks the final normal base package's native startup, public-channel status and absence of test facilities. Keep locator files, fixture profiles, logs, captures and results out of source and distribution. Controlled provider fixtures do not constitute authenticated provider-account acceptance.

For every shared-code change, run the Windows Rust tests and relevant native application tests as well as the Mac checks. Before committing, inspect the exact index and full staged diff and run the repository's staged security gate. Before distribution, reconcile all approved features and test the exact final packages on both platforms.
