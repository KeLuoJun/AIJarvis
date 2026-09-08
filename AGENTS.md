# Repository Guidelines

## Project Structure & Module Organization

AI Jarvis is a Windows-only Tauri v2 application. The React/TypeScript frontend lives in `src/`: reusable UI is in `components/`, window views in `windows/`, hooks in `hooks/`, and IPC/types/helpers in `lib/`. Rust code and native integrations are under `src-tauri/src/`; integration and contract tests are in `src-tauri/tests/`. Runtime prompts, schemas, and pinned model configuration belong in `resources/`. Pet artwork is authored in `assets/pet/` and packaged from `public/assets/pet/`. Environment and model utilities live in `scripts/`; design records live in `docs/`.

## Build, Test, and Development Commands

- `pnpm install --frozen-lockfile`: install the locked JavaScript dependencies.
- `pwsh -NoProfile -File scripts/setup.ps1`: check Node, Rust, WebView2, capture APIs, llama.cpp, and model files.
- `pnpm tauri dev`: prepare the ASR sidecar and launch the desktop app with Vite.
- `pnpm build`: run strict TypeScript checking and create the frontend bundle.
- `pnpm test`: run all Vitest tests once; use `pnpm test:watch` while developing.
- `cargo test --manifest-path src-tauri/Cargo.toml`: run regular Rust unit and integration tests.
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check` and `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`: enforce Rust formatting and warning-free code.
- `pnpm tauri build --debug --no-bundle`: produce the internal debug executable without an installer.

## Coding Style & Naming Conventions

TypeScript is strict and uses two-space indentation, double quotes, semicolons, `PascalCase` React components, `camelCase` functions, and kebab-case helper filenames such as `window-label.ts`. Keep Tauri calls behind `src/lib/ipc.ts`. Follow `rustfmt` defaults; use `snake_case` for Rust modules/functions and `PascalCase` for types. Preserve bounded queues, explicit timeouts, and structured IPC arguments in native code.

## Testing Guidelines

Name frontend tests `*.test.ts` or `*.test.tsx` beside the code they cover. Put Rust cross-module behavior in `src-tauri/tests/` and focused unit tests in the owning module. Add regression tests for IPC contracts, privacy transitions, lifecycle cleanup, and capacity limits. Real Qwen and SenseVoice contract tests are ignored by default; run their documented `--ignored` commands only with pinned models and licensed WAV fixtures installed. Never replace those gates with fake transcripts.

## Commit & Pull Request Guidelines

Use the repository's imperative Conventional Commit pattern, for example `feat: add bounded scheduler` or `fix: preserve runtime DLLs`. Keep commits scoped to one behavior. Pull requests should explain user-visible and privacy impact, list verification commands, link the relevant issue or checklist item, and include screenshots for control, pet, or danmaku UI changes. Do not commit models, generated binaries, recordings, `dist/`, or `target/`.
