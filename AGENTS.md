# Workflow development

- Independent Rust/Tauri application. Do not modify another development project or use its data profile.
- Provider terminals belong to this application. Closing the application stops its terminals. No persistent terminal service, background daemon, tray hiding, or automatic provider restart.
- Persist projects, session metadata, transcripts, layout and conversation identifiers. Reattaching a view must not send input or change a provider conversation.
- Git is optional. Use Git worktrees when HEAD exists, isolated folder copies otherwise. Never initialize a project repository automatically.
- Keep credentials and signing keys out of source and distribution. Do not copy account caches into packages.
- Implement our own application code. Test terminal lifecycle, browser selection and browser actions before distributing.
- No local commits or remote pushes unless explicitly requested.

