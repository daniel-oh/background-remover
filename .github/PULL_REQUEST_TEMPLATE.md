**What this changes, and why**

**Checks**

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --release` with `MODEL_PATH` set, both golden fixtures
- [ ] If the image path changed: the golden numbers, before and after, are in this description
- [ ] If a dependency was added: a sentence on why it earns its place
