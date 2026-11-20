# Workflow development

- Independent Rust/Tauri application. Do not modify another development project or use its data profile.
- Provider terminals belong to this application. Closing the application stops its terminals. No persistent terminal service, background daemon, tray hiding, or automatic provider restart.
- Persist projects, session metadata, transcripts, layout and conversation identifiers. Reattaching a view must not send input or change a provider conversation.
- Git is optional. Use Git worktrees when HEAD exists, isolated folder copies otherwise. Never initialize a project repository automatically.
- Keep credentials and signing keys out of source and distribution. Do not copy account caches into packages.
- Implement our own application code. Test terminal lifecycle, browser selection and browser actions before distributing.
- No local commits or remote pushes unless explicitly requested.
- For every shared provider feature, inspect and test Codex, Claude Code and GitHub Copilot together. Cover provider capabilities, live interaction, conversation continuity and failure paths for all three; do not finish one provider and silently omit the others. Distinguish controlled CLI fixtures from verification with a signed-in real provider.

## Release scope

- Before preparing a release, reconcile all user-approved changes against the release source, including approved work still in the local development copy. A previous push limited to specific changes does not define the next release's scope.
- Account for each approved feature in the release candidate and verify it in the packaged executable. If an approved change is absent from the intended source or blocked, resolve that gap before publishing. Keep unapproved changes outside the release.
- Verify the final Windows ZIP and the executable's version and features. Use the tested artifact for publication and check that the public downloads match its checksum.
- For terminal clipboard acceptance, test copying into and pasting from the real Windows clipboard using the packaged executable. A synthetic WebView paste event is supplementary coverage.

## Mandatory security gate before every commit

- Before EVERY commit, review the exact staged file list and full staged diff, then run `node scripts/check-commit.cjs --staged` in the actual repository. Repeat after any staging change. A worktree scan or an earlier passing result is not a substitute for checking the final index.
- Never commit credentials, API/access/refresh tokens, passwords, signing/private keys, `.env` files, account caches, browser profiles/cookies, session metadata, conversation identifiers/transcripts, clipboard contents, runtime databases, logs, dumps, or generated test inputs/outputs/captures. Do not print secret values in reports or terminal output.
- Test source code may generate synthetic data at runtime in ignored temporary locations. Never copy real accounts, environment values, user conversations or captured runtime data into fixtures. Do not exempt test files or vendor files from secret scanning.
- Inspect new or changed binary/media files separately for private data, including every frame of animations. The automated gate rejects new or changed binary files; do not bypass that failure without an explicit, reviewed policy change authorized by the user.
- Treat scanner errors, unreadable files, suspicious literals, unknown binary files, and an unavailable Git index as blocking conditions. Resolve each finding before committing; do not bypass hooks with `--no-verify`, disable the scanner, or add broad allowlists to silence findings.
- `.gitignore` is not sufficient: already tracked or force-added files must also be checked. Never delete user data merely to make an audit pass; keep it outside the index and in ignored storage.
- Use the versioned `.githooks/pre-commit` hook in real Workflow repositories (`git config core.hooksPath .githooks`). If Git is absent, do not initialize it: audit with `node scripts/check-commit.cjs --worktree` and explicitly report that staged/history verification and hook activation are unavailable.
- This gate does not authorize a commit or push. Explicit user authorization is still required.

