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

The normal application is `target/release/bundle/macos/Workflow.app`. The local packaging script creates `dist/Workflow-Update-macOS-arm64.app.zip` and its SHA-256 file. It verifies bundle identity, version, Apple Silicon executable, embedded update verification key, code signature, dependency notices, resource allowlist, and absence of the desktop test bridge. It neither publishes a release nor signs an update manifest.

The Mac configuration uses the rounded `icons/icon-macos.png` and Retina `icons/icon-macos@2x.png`. Before both Tauri builds and development runs, `scripts/icons-macos.sh` uses macOS tools to generate `target/macos-icons/Workflow-Mac.icns` at standard and Retina sizes. The explicit ICNS entry also prevents development mode from falling back to the shared Windows PNG. Finder, Dock and Launchpad all use this bundle icon; the Windows icon assets remain separate. Keep one installed `Workflow.app` in a stable Applications location.

Existing Launchpad bookmarks can follow older test copies even after those copies move outside application search locations and their Launch Services entries are unregistered. Verify that the visible Launchpad entry resolves to the current installed application and that its cached icon matches the installed bundle. Any machine-specific repair must have a private rollback backup, affect only Workflow entries, and preserve other applications' icons, folders and positions. Keep runtime caches, bookmarks and repair backups outside source and packages. A custom acceptance profile still requires its existing launcher; launching the bundle directly does not inherit the launcher's environment. Do not move account caches or embed local profile settings in the application to refresh its icon.

The build and package scripts create an **ad-hoc signed local candidate**. That signature is not an Apple Developer ID signature and does not provide notarization. For public distribution, use an authorized Apple signing environment, notarize the finished bundle, staple the ticket, and validate Gatekeeper behavior on a separate machine. Do not treat the local ZIP as a public release or disable platform security checks to make acceptance pass.

## Native application tests

Build a separate debug-only application with `desktop-tests` explicitly enabled:

```sh
target/devtools/node_modules/.bin/tauri build --debug --bundles app --features custom-protocol,desktop-tests -- --locked
mkdir -p target/test-tools
rustc tests/provider-fixture.rs -o target/test-tools/provider-fixture
node tests/macos.cjs
```

`WORKFLOW_MAC_TEST_EXE` may select another debug test application. The suite uses temporary project and provider profiles, controlled CLI fixtures, authenticated local MCP routing, real WKWebView input, and the real system Pasteboard. It preserves the original clipboard items and types and stops if they cannot be safely restored. Test data and captures must stay in ignored or temporary storage.

The test bridge requires both a debug build and the explicit feature, plus test mode and an isolated profile at runtime. A release build with `desktop-tests` must fail to compile. Never distribute the test application.

Controlled providers verify application mechanics; they do not establish that an installed, signed-in CLI works. Separately test Codex, Claude Code, and GitHub Copilot CLI with their own accounts, current model capabilities, conversation resumption, approval cards, custom question replies, and file/image attachments. Attaching a view, changing tabs, and reading history must not start providers or send input. Closing the app must stop only its owned processes; reopening must not automatically restart them.

Also verify Finder file and image drops, screenshot paste in both Terminal and Chat, Dock and popup behavior while minimized/backgrounded, and application close/quit paths. Teams requires a signed-in Mac Teams Web connection and an explicitly authorized destination and message. Do not send a real message merely to satisfy a fixture test.

## Updates and Windows compatibility

Mac updates use `Workflow-Update-macOS-arm64.app.zip` and the signed `Workflow-Update-macOS-arm64.json`. They share the pinned public verification key with Windows but use a platform-specific package and manifest. Apple application signing and update-manifest signing are separate checks. A signature that authorizes a Windows asset cannot authorize a Mac installation.

The update signing key stays in the designated Windows release account's non-exportable key store. After reviewing the final Mac ZIP, that account can sign its manifest with `scripts/sign-update.ps1 -Platform macOS-arm64` and the script's required package/release arguments. Never export the private key or put it in a repository, package, or Mac profile.

Until a signed Mac manifest is published through an authorized release, the public update channel cannot install this local candidate. Test replacement and failure recovery on disposable app copies. Keep existing Windows asset names, signature format, stable update feed, and immutable releases unchanged. A Mac preview must not change which release Windows clients receive.

For every shared-code change, run the Windows Rust tests and relevant native application tests as well as the Mac checks. Before committing, inspect the exact index and full staged diff and run the repository's staged security gate. Before distribution, reconcile all approved features and test the exact final packages on both platforms.
