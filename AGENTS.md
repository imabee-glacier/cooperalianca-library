# Repository rules

Commit, push, open a PR, or merge only when the task fully meets its specification
and every required check passes. If a check fails or did not run, do not ship;
report what is unmet.

Required checks:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --all-targets --locked`
- `cargo test --doc --locked`
- `cargo build --release --locked`
- `cargo audit --deny warnings`

Keep credentials, cookie jars, keys, password source files, and private portal
responses outside this repository and artifacts. Use fake fixtures in tests.
The SDK must remain direct HTTP with no browser dependency or fallback.
Do not add guessed endpoints or claim live verification without evidence.
