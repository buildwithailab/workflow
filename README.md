# Workflow

[![Latest release](https://img.shields.io/github/v/release/buildwithailab/workflow?sort=semver&label=release)](https://github.com/buildwithailab/workflow/releases)
[![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4)](#quick-start)
[![macOS 14+ on Apple Silicon](https://img.shields.io/badge/macOS-14%2B%20%7C%20Apple%20Silicon-000000)](docs/macos.md)
[![MIT License](https://img.shields.io/github/license/buildwithailab/workflow)](LICENSE)

Workflow is a desktop workspace for Codex, Claude Code, and GitHub Copilot CLI. Arrange their terminals on a map, draw links that grant specific access, and follow the work from a conversation to a browser check and a code review.

Windows x64 and Apple Silicon macOS downloads are available on the [Releases page](https://github.com/buildwithailab/workflow/releases). See the [macOS guide](docs/macos.md) for platform requirements, local builds, and update behavior.

Start with one session and add more as needed. This example uses five.

![Example workflow for an issue status filter. Implementation reads Requirements and requests work from API Review, Browser QA, and Release Notes. API Review has a separate link to read Implementation files.](docs/images/issue-filter-example.svg)

[Session links](#share-context-across-sessions) · [Workflow MCP](#how-workflow-mcp-works) · [Browser testing](#from-local-preview-to-browser-evidence) · [Add-ons](#use-add-ons-in-selected-sessions) · [Quick start](#quick-start)

## Share context across sessions

The example adds an **Open / Closed / All** filter to a local issue tracker. The selected status should survive a reload through the URL, and the API should reject invalid status values. Implementation owns the code and serves the preview from its workspace. The other four sessions contribute requirements, findings, test evidence, or release notes.

| Session | Role | Outgoing link |
| --- | --- | --- |
| **Requirements** · Claude Code | Defines acceptance criteria and edge cases, including URL persistence and empty results. | — |
| **Implementation** · Codex | Builds the filter and API, starts the preview, and applies fixes from review. | Read the **Requirements** conversation. Request work from **API Review**, **Browser QA**, and **Release Notes**. |
| **API Review** · Codex | Reads the changed endpoint and returns findings about validation and the response contract. | **Implementation** · Read files |
| **Browser QA** · GitHub Copilot | Opens Implementation's preview in the built-in browser and reports the results. | — |
| **Release Notes** · Claude Code | Drafts a user-facing note from the verified behavior supplied in the request. | — |

The API review uses two links. Implementation can request a review, and API Review can read the files it needs through a separate link in the opposite direction. Browser QA gets the preview address and test steps in its work request. Release Notes gets the verified behavior in its own request.

On the map, drag from the calling session's link icon to the session it should access. Choose permissions when saving the link. The arrow points **from the caller to the session it can access**. Click the link icon on the line to inspect its permissions.

### Watch the browser QA handoff

The animation follows the five-session example as Implementation grants **Request work** to Browser QA through one link. Browser QA submits the accepted request, checks the filter in the built-in browser, and returns the result through that same request.

![Five Workflow sessions and their five directional links. A permission guide highlights the single Implementation to Browser QA link. Close-ups follow the request, browser checks after reload, and the completed result.](docs/images/workflow-browser-qa-walkthrough.png)

*Recorded in Workflow against a local demo with sample terminal output. The walkthrough adds a permission guide and close-ups to the app capture.*

<details>
<summary>See the five-session workspace</summary>

![Five sessions on the Workflow map. Implementation has one straight link to Browser QA. Two separate arrows connect Implementation and API Review with Request work and Read files permissions.](docs/images/workflow-five-session-workspace.png)

*Workflow running a local demo with sample terminal output. Arrow routing and permission labels are arranged for this capture.*

</details>

## How Workflow MCP works

When session tools are enabled, Workflow configures the provider CLI to use its local Model Context Protocol (MCP) server. The CLI calls a stdio adapter, which forwards the request to the running desktop app over a loopback connection. Workflow checks the calling session and its current run before handling the call. Connected-session permissions and browser access are checked separately.

![MCP call route from a provider CLI through the local stdio adapter to Workflow, where connected-session and browser permissions are checked separately.](docs/images/mcp-routing.svg)

| What the agent needs | Workflow tools | Access |
| --- | --- | --- |
| Find connected sessions | `session_catalog` | Lists the caller's explicit links |
| Read a conversation | `read_context` | Read conversation permission on the target link |
| Inspect changed files or read selected files | `read_files` | Read files permission on the target link |
| Ask for work and exchange results | `request_work`, `report_result`, `read_result` | Request work permission and user acceptance |
| Use a browser and record checks | `browser_open`, `browser_read`, `browser_action`, `browser_assert` | Browser access for the calling session |
| Send a Teams message | `teams_send_message` | A signed-in Teams Web connection and a matching chat link saved for the calling session |
| Show the Teams window | `teams_open` | An existing Workflow Teams Web window and a user request to see it |

A work request goes to an existing session. When you accept it in Workflow, it becomes a draft in that session's terminal. You submit the draft, and the receiving agent can report the outcome for the originating session to read.

To send a Teams message from an agent session, choose **Connect** in the Teams sidebar row. Once it shows **Connected**, select the session and click the pencil icon in the Teams row. You can also use the session card's **⋯ → Teams destination** menu. Enter a chat link or choose **Chat with myself**, then save. Ask the agent to send a message to the saved Teams chat and provide the text. Workflow checks the saved destination and submits the message through the hidden Teams Web window. Check the conversation in Teams to confirm delivery.

Choose **Open** in the Teams row or ask the agent to show Teams to inspect the existing Teams Web window. If an unsent draft blocks a send, Workflow shows that window for you to review the draft. Close the Teams window to hide it again.

## From local preview to browser evidence

**Launch the project from its session.** Save a PowerShell command and a local URL on the session card. **Preview** runs the command in that session's workspace and opens the built-in browser after verifying that the server is ready. If the port is already in use, Workflow tells you before launching.

### Send a prompt about a selected element

In the built-in browser, turn on **Pick element** and hover over the inventory page’s download icon. Workflow outlines the button and shows its selector, `button#export-csv`. Click the highlighted button, then choose **Add to prompt** to attach references to its HTML, styles, and screenshot. Add your request in the terminal and press **Enter** to send it.

> Add a visible “Export CSV” label next to the download icon.

![Workflow’s built-in browser highlights a blue export button on a light inventory page and shows its selector. The walkthrough adds the element context to a session draft and sends a request for a visible Export CSV label.](docs/images/workflow-inventory-element-picking.png)

*Recorded in Workflow with sample inventory data and a test terminal. The close-up shows the picker’s highlight and selector. The prompt excerpt shows the request and attachment filenames.*

### Record a browser check

**Ask the agent to verify what happens.** Browser QA can open the preview URL in its own session, select **Closed**, reload the page, and check both the URL and the visible results. Open **Test steps** to inspect the recorded actions and passed or failed checks. Workflow can capture a screenshot when a check fails.

For checks in Chrome, Workflow can open a tab in a dedicated Chrome profile.

> Open the preview in the built-in browser and select Closed. Reload the page. Verify that the URL still contains `status=closed`, the page shows closed issues, and open issues are excluded.

<img src="docs/images/issue-filter-test-steps.png" width="520" alt="Browser QA's test steps in the built-in browser showing the Closed selection and three passed checks after reload.">

*The trace displays the base address for privacy. The browser address bar in the walkthrough shows the full filter URL.*

## Review the work before you merge

| Surface | What you can review or change |
| --- | --- |
| **Session Summary** | Files changed since the session started, line diffs, linked sessions, and browser verification |
| **Files and Changes** | Edit supported text files, inspect diffs, revert selected lines, or transfer selected changes from an isolated workspace back to the project |
| **Git** | Stage and commit selected files, see the branch graph, compare two commits, and resolve merge conflicts |
| **Providers** | Choose a CLI account per session and view the usage limits each provider reports |
| **Add-ons** | Install supported skill packages from the Add-on Library and enable them per isolated session. The current catalog includes [OpenSpec](https://github.com/Fission-AI/OpenSpec) and [Ponytail](https://github.com/DietrichGebert/ponytail). |

The Add-on Library previews the pinned version, installed skill files, and any setup command before installation. Workflow verifies the Windows x64 OpenSpec download before running its initializer, and verifies each skill from the pinned Ponytail revision. Existing installations can be upgraded from the library.

Workflow MCP checks the target project's and session workspace's `.workflowignore` rules before sharing a file. It also blocks common sensitive filenames and detected credentials. The local warning before opening a session shows paths only. A provider's own shell and file tools follow that provider's access settings.

Choose **Current folder and branch** to work directly in the project. Choose **Separate workspace** to work in a Git worktree when the project has an initial commit, or a folder copy otherwise. Workflow also supports projects without Git.

From a Git session, open **Changes → Pending Git changes → ⋯ → Merge into target branch** to merge its committed work into a local target branch. You do not need another provider session in the original project. Workflow checks the source and target, tries the merge in a temporary workspace, then updates the target while preserving the source session. Uncommitted changes, active target sessions, and stale previews block the operation. If conflicts occur, the target stays unchanged; resolve them in the source session and retry. This action does not push to a remote.

## Use add-ons in selected sessions

Open the **Add-on library** beside **Projects** and install a package. In **New session**, choose **Separate workspace**, check the add-ons you want, and select **Start session**. Installed add-ons are unchecked by default in each new session.

![Workflow walkthrough showing Ponytail installed from the Add-on library, enabled for a Code Review session, and left unchecked in the next new session.](docs/images/workflow-session-addons.png)

*Recorded in Workflow with a local demo project and a test terminal.*

## Quick start

1. Download `Workflow-Windows-x64.zip` from the repository's **Releases** page.
2. Extract the archive and launch `Workflow.exe`.
3. Open **Providers** to find your CLI, select its executable, or sign in.
4. Add a project folder or clone a repository, then start a session. The interface language is available in **Settings**.

**Requirements:** Windows 10 or 11 (x64), Microsoft Edge WebView2 Runtime, and the CLI and account for each provider you use. Git enables Git operations, Google Chrome enables Chrome mode, and GitHub CLI enables draft pull requests. Provider CLIs and add-ons can have additional requirements. Workflow itself ships as a Rust/Tauri executable with bundled frontend assets.

For Apple Silicon macOS, download the macOS `.app.zip` from [Releases](https://github.com/buildwithailab/workflow/releases), move `Workflow.app` to **Applications**, and follow that release's first-launch instructions. See the [macOS guide](docs/macos.md) for requirements and current limitations.

## Local data and session lifetime

### Terminal copy and paste

Paste text from other Windows applications with **Ctrl+V**, **Ctrl+Shift+V**, or **Shift+Insert**. Select terminal text and use **Ctrl+C**, **Ctrl+Shift+C**, or **Ctrl+Insert** to copy it. With no selection, **Ctrl+C** still interrupts the CLI. Right-click opens Copy/Paste; when a CLI captures mouse input, hold **Shift** while right-clicking.

Terminals that enable bracketed paste keep the original line breaks. Otherwise, Workflow asks before replacing line breaks and tabs with spaces. These shortcuts also work in provider sign-in terminals.

### Session storage

Workflow stores projects, session metadata, terminal output, layout, and conversation IDs locally. Closing the app stops its terminals and preview servers. Reopening restores the workspace, and the play button on a session card resumes a verified conversation. The default profile is `%LOCALAPPDATA%\local.workflow.desktop`. Set `WORKFLOW_DATA_DIR` before launch to use a separate profile.

## Session notifications

Workflow shows a single card when work finishes or a session needs an approval or answer. Project badges and the Windows taskbar indicator highlight waiting sessions. Waiting badges stay until you respond; dismissing a card does not answer the question. Click a card to open the existing session.

When Workflow is minimized or in the background, a small desktop card appears without taking focus. It shows only the Workflow name and a general status. The session you are following stays quiet while its window is in the foreground. Turn cards and attention badges on or off in **Settings → Session notifications**.

## Updates

Open **Settings → About and updates** to see the installed version, check for a newer release, and read the release notes. Automatic updates are on by default and can be turned off here. Workflow checks once a day while it is open. When a verified update is ready, choose **Update and restart** from the notification or the settings screen, or let Workflow install it when you close the app. Restarting stops open terminals and keeps your existing profile.

## Build and test

Install the Rust toolchain pinned in `rust-toolchain.toml`, the MSVC build tools, and the Windows SDK. From the project root:

```powershell
.\Build.cmd
cargo test --locked
.\scripts\package.ps1 -ReleaseNotes <release-notes-file>
```

The packaging script produces `dist/Workflow-Windows-x64.zip` and a SHA-256 checksum. The integration suite in `tests/integration.cjs` uses Playwright Core and a temporary data profile to exercise Windows terminals, Git, browser actions, previews, and shutdown. Set `WORKFLOW_TEST_EXE` to the executable to test and `WORKFLOW_PLAYWRIGHT` to the Playwright Core module path if installed outside the repository.

For Apple Silicon builds, use the [macOS guide](docs/macos.md). Platform modules share the session, conversation, permission, and Git behavior; a Mac test result does not replace Windows acceptance testing.

### Publish an update

Match the version in `Cargo.toml`, `Cargo.lock`, and `tauri.conf.json` to the intended stable GitHub tag. Before the first signed build, run `powershell -NoProfile -File scripts/init-update-signing.ps1 -Create` on the designated Windows release account. This creates a non-exportable P-256 signing key in that account's Windows key store and writes only its public verification key to `src/update-public-key.txt`. Build after pinning the public key. Other build machines can use that public key, but only the designated release account can sign updates. Losing that account/key requires distributing a manually installed build with a new trusted public key; it cannot be silently rotated or recovered from the repository.

Package with `powershell -NoProfile -File scripts/package.ps1 -Executable <built-exe> -ReleaseNotes <release-notes-file>`. The packaging step signs and locally verifies `dist/Workflow-Update-Windows-x64.json`, binding the stable version, executable name, size, SHA-256, repository, and release notes. It rejects a signing key that differs from the app's pinned public key and an executable built without that pin. No GitHub token or private signing key belongs in source or distribution.

Keep immutable releases enabled. Attach the Windows ZIP, its SHA-256 file, `Workflow-Update-Windows-x64.exe`, and the signed `Workflow-Update-Windows-x64.json` while the release is still a draft, then publish. Test the exact packaged executable and verify all four public downloads against the tested artifacts. Workflow checks the public latest-release JSON asset directly, without GitHub REST API requests or user login, and verifies its signature before trusting release data. Missing or invalid metadata is reported as an error, never as an up-to-date result. Existing immutable releases without this JSON file remain downloadable manually; they must not be edited. Older clients retain their original REST update path until upgraded.

## License

Workflow is available under the [MIT License](LICENSE). See the [third-party notices](THIRD-PARTY-NOTICES.txt) for dependency licenses and attributions.

