# servo-si

`lace` is a small Rust program using OpenAI's Codex core. It starts one parent
conversation, asks that agent to spawn two child conversations, waits for both
children to reply `Hi!`, and prints their replies and thread IDs. These are Codex
conversation threads, not operating-system threads.

With Rust, sccache, and an existing Codex login (`codex login`) installed:

```sh
cd lace
cargo run --locked
```

Lace uses your Codex configuration and login. Optionally select a model with
`--model MODEL` or adjust the 120-second run timeout with `--timeout-secs SECONDS`:

```sh
cargo run --locked -- --model MODEL --timeout-secs 180
```

The demo uses read-only permissions and shuts down its agent threads when it
finishes, fails, times out, or receives Ctrl-C. It exits with an error if the
parent does not produce exactly two completed child greetings.

```sh
cargo test --locked
cargo fmt --check
```
