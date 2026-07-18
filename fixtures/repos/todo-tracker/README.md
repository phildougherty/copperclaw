# todo-tracker

A tiny command-line todo tracker used as a C2 attach-flow fixture: a
small, self-contained multi-file repository that stands in for an
*existing* project the agent is asked to open and change (rather than
build from scratch).

It stores todos in a JSON file and exposes add / list / done verbs. The
code is deliberately plain so the fixture stays legible; its value here
is structural — a real `package.json` with named scripts, a `src/` tree,
and a README — so the attach flow has genuine signal to infer verify
stages and seed the decision log from.

## Usage

    node src/index.js add "buy milk"
    node src/index.js list
    node src/index.js done 1
