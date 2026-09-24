# rust-node

`rust-node` is the start of a Node-compatible runtime written in Rust.

## Current scope

This repository now includes:

- a Rust CLI binary named `runtime`
- JavaScript execution backed by `boa_engine`
- `console.log(...)` support through `boa_runtime`
- local CommonJS loading for relative files
- a minimal in-process timer queue for `setTimeout()` and `clearTimeout()`

```bash
cargo run --bin runtime -- ./hello.js
```

Example:

`answer.js`

```js
module.exports = 42;
```

`main.js`

```js
const answer = require("./answer");

console.log(answer);

setTimeout(() => {
  console.log("done");
}, 0);
```

## Architecture direction

The runtime is now organized around a reusable execution context:

```text
Runtime
├── CLI
├── JS Engine
├── Runtime Context
│   ├── console
│   ├── module cache
│   └── timer callback store
├── Module System
│   └── CommonJS wrapper + relative resolution
├── Scheduler
│   └── simple task queue for timers
└── Future Node APIs
```

## Roadmap

- [x] Phase 1: JavaScript execution
- [x] Phase 2: runtime scheduling and CommonJS foundation
- [ ] `process`
- [ ] `path`
- [ ] `fs`
- [ ] fuller event loop behavior
- [ ] npm package resolution
- [ ] broader Node API compatibility

Phase 2 is still intentionally narrow. It establishes a long-lived runtime instance, local CommonJS modules, module caching, and timer scheduling without introducing Tokio, networking, or full npm compatibility yet.