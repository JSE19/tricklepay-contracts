Summary of changes for issue #61 (Test withdrawal at exact end)

- Added test: `withdraw_at_exact_end` in `contracts/stream/src/test.rs`.
- Added counter-boundary tests to ensure `u64::MAX` is handled and ids are not reused.
- Made test helper `try_create_stream_for_raw` robust to SDK client return-shape changes.
- Documented failure mode in `fix.md`.
- Ran `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` successfully.

Files changed (high level):
- `contracts/stream/src/test.rs` (tests and helper updates)
- `fix.md` (failure mode documentation)

Suggested commands before committing locally:

```bash
# run the full test suite
cargo test

# check formatting and lints
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Notes:
- The contract already fails closed on id exhaustion using `checked_add` and returns `StreamError::StreamCountExhausted`.
- I avoided committing changes; please review and commit locally as you prefer.
