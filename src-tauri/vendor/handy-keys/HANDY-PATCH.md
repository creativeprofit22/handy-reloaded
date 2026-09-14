# Handy's local handy-keys 0.3.4 patch

## Provenance

Source: the published `handy-keys` 0.3.4 crate, MIT licensed (see LICENSE).
Upstream: https://github.com/cjpais/handy-keys
Original crates.io checksum:
`1a007b6c921d3273fd88aac45b516eee01e5a9dc2d76bb6f1a981500cc0d818a`

Before copying, every published file was compared with the locally cached archive, and the archive's SHA-256 was checked against Handy's original Cargo.lock. No dependency was downloaded and no shared Cargo cache source was edited.

The application selects this copy through its existing `[patch.crates-io]` table. Its lockfile changes only handy-keys' source, not the version or other dependencies. This crate's own standalone-test lockfile was refreshed offline to versions already cached; it does not control Handy's dependency resolution.

## Windows readiness fix

The original Windows listener constructor returned as soon as it spawned a thread, before that thread installed its native hooks. Handy's manager-thread readiness handshake therefore could return success before `SetWindowsHookExW` ran, intermittently losing the first shortcut press.

The hook-owning thread now acknowledges readiness after keyboard/mouse installation and watcher setup, immediately before entering the unchanged message loop. Native hook setup errors reach the constructor. Startup thread failure/disconnection is an error, not success; error paths join the thread before returning, including partial keyboard cleanup after mouse-hook installation failure. Thread creation errors are propagated as I/O errors.

No hook filtering, shortcut matching, event delivery, permission behavior, message-loop timing or macOS/Linux code was changed. Handy's outer readiness handshake is preserved. There are no diagnostic buffers or debug prints in this patch.

## Regression checks

From the Handy repository root:

```sh
cargo test --manifest-path src-tauri/vendor/handy-keys/Cargo.toml --locked --offline --lib --target-dir src-tauri/target
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib
cargo clippy --manifest-path src-tauri/Cargo.toml --locked
```

Windows test-only setup seams exercise delayed hook setup, keyboard setup failure, mouse setup failure followed by successful retry, and startup-thread panic. Real native handles created by tests are shut down and joined. Handy's existing native test additionally verifies actual key-down/key-up delivery and transactional backend switching.

Remove this patch only after a replacement upstream version provides an equivalent native readiness guarantee and passes these checks. Do not substitute sleeps or longer receipt timeouts.
