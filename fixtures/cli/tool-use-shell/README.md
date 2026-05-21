## cli / tool-use-shell

One CLI inbound (`run 'echo hello'`) drives two Claude turns: the first
emits a `tool_use` block requesting the `shell` tool with
`command: "echo hello"`; the runner executes it (real bash, deterministic
output) and feeds the `tool_result` back; the second turn streams the
final assistant text. Asserts that the runner's tool-use outer loop
round-trips through one full tool call within a single inbound.

This is the M11 smoke test for tool use; future fixtures can cover the
remaining built-in tools.
