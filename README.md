# rust-node

`rust-node` is the start of a Node-compatible runtime written in Rust.

## Current scope

This repository now includes a minimal Phase 1 runtime:

- a Rust CLI binary named `runtime`
- JavaScript execution backed by `boa_engine`
- `console.log(...)` support through `boa_runtime`

```bash
cargo run --bin runtime -- ./hello.js
```

## Architecture direction

The long-term plan follows the issue outline:

```text
                 JavaScript App
                        │
                Node.js API Layer
                        │
        ┌───────────────┼───────────────┐
        │               │               │
      fs/net       process/worker    modules
        │               │               │
        └───────────────┼───────────────┘
                        │
                    Rust Core
                        │
          ┌─────────────┼─────────────┐
          │             │             │
        JS VM         Tokio         OS APIs
```

Phase 1 is intentionally small: execute JavaScript files and establish the CLI entry point that future work can extend with timers, modules, and Node-compatible built-ins.