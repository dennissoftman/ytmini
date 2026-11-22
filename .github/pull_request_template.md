<!-- Title as a Conventional Commit: "fix: …", "feat: …" (it becomes the changelog entry). -->

**What changes for the user**

**How it was tested** (macOS / Windows; `YTMINI_NO_HISTORY=1` for anything that plays)

- [ ] `cargo fmt`, `cargo clippy --release --all-targets -- -D warnings` and `cargo test --release` pass
- [ ] New UI strings go through `t()` and are in every `src/i18n/*.rs`
- [ ] No cookies, tokens or stream links in logs or output
