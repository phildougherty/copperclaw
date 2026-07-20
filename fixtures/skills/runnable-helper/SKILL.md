---
name: runnable-helper
description: A minimal skill that ships an executable helper script under scripts/, used to prove M22 S1 materialize places a skill's supporting files into the container's /data/skills so the helper can actually run.
---

# runnable-helper

A fixture skill for M22 S1 (wire `materialize` into container spawn).

Unlike a pure-prose skill, this one carries a real helper under
`scripts/greet.sh`. Once the host materializes the selected skills into the
session's `/data/skills`, the agent can execute it:

```sh
sh /data/skills/runnable-helper/scripts/greet.sh
```

If the script runs, the skill's `scripts/` reached the sandbox — the whole
point of S1. Before S1 only this markdown body reached the agent (via the
system prompt / `skills.json`); the script never crossed the boundary.
