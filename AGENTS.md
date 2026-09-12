# Coding agent instructions

React/TypeScript frontend with Bun; Tauri 2/Rust desktop backend.
See [CLAUDE.md](CLAUDE.md) for detailed agent-behavior guidance.

Run commands from this repository root:

- Install: `bun install --frozen-lockfile`.
- Build frontend: `bun run build`.
- Test frontend: `bun run test:playwright` (requires Playwright Chromium).
- Lint frontend: `bun run lint`.
- Check formatting: `bun run format:check`.
- Build desktop: `bun run tauri build` (prerequisites in [BUILD.md](BUILD.md)).
- Test Rust: `cargo test --manifest-path src-tauri/Cargo.toml`.
- Lint Rust: `cargo clippy --manifest-path src-tauri/Cargo.toml`.

CI lives in `.github/workflows/ci.yml` and must stay green.
CI checks the frontend; native builds require platform libraries and models.
Inherited workflows are disabled in GitHub to avoid cross-platform builds and uploads.
Bun uses its own dependency cache because setup-node does not support Bun caching.
Never commit with `--no-verify`; fix failed checks instead of bypassing them.
Preserve the upstream project license and existing work; use conventional commit prefixes.
