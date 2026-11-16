# Contributing to Workflow

File a [Bug Report or Feature Request](https://github.com/buildwithailab/workflow/issues/new/choose) to share a reproducible problem or propose a change. For larger changes, discuss the scope in an issue before implementation. Use the [security policy](SECURITY.md) for vulnerability reports.

## Build and test on Windows

Use Windows 10 or 11 (x64) with Git, Rustup, the MSVC build tools, the Windows SDK, and the Microsoft Edge WebView2 Runtime. Rustup reads the pinned compiler version from `rust-toolchain.toml`. Install Node.js for the commit security check and the optional desktop integration suite.

```powershell
.\Build.cmd
cargo test --locked
```

The [desktop integration suite](tests/integration.cjs) uses Playwright Core and a temporary Workflow profile. See [Build and test](README.md#build-and-test) for the executable and module path options. For changes to terminals or the built-in browser, check the affected behavior on Windows and record what you verified.

## Submit a pull request

1. Fork the repository and create a branch from `main`.
2. Keep the change focused and run the relevant tests.
3. Configure the repository's commit hook from the checkout with `git config core.hooksPath .githooks`. Review the full staged diff and run `node scripts/check-commit.cjs --staged` on the final staged changes before every commit. Repeat the review and check if you restage anything.
4. Push to your fork and open a pull request against `main`. Describe the user-visible change, link the related issue if there is one, and list the tests you ran.

Keep credentials, keys, local environment files, browser profiles, account caches, and real project or session data out of contributions. Generate synthetic test data at runtime and keep test output, captures, and build artifacts out of commits.
