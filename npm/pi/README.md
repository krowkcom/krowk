# @gruntas/pi

Gruntas for [pi](https://pi.dev). Each pi event the extension handles runs
`gruntas hook pi <event>`, the entry point gruntas's Claude Code and Codex
plugins run, so a pi session is treated as theirs are.

```bash
pi install npm:@gruntas/pi
```

It needs `gruntas` on the PATH: without it, a session's start says how to
install it and the extension does nothing else. On Windows it does nothing.

Supported pi: 1.6 and later 1.x (`peerDependencies`).

MIT.
