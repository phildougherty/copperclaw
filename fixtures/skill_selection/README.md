# fixtures/skill_selection — M22 S2 relevance-selection fixture set

Four topically-disjoint skills used to prove `SkillsSelector::Relevant`
(M22 S2) narrows the inlined skill set for an off-topic query, instead of
splicing in all of them the way `SkillsSelector::All` does:

- `bread-baking` — sourdough / dough / loaf / oven.
- `astronomy-observing` — telescope / planets / nebulae / night sky.
- `tax-filing` — income tax return / deductions / filing deadline.
- `garden-care` — seedlings / soil / pruning / pests.

The descriptions share no salient vocabulary, so a query about one topic
(e.g. "how do I bake a sourdough bread loaf") FTS-matches that skill's
description and not the others. The SX X-rider
(`crates/copperclaw-skills/tests/coverage.rs::relevant_selector_narrows_inlined_skill_set`)
scans this set through `SkillRegistry`, compares `Relevant` vs `All`, and
asserts the relevant subset is strictly smaller and cheaper to inline — the
prompt-shrink S2's decision (e) promises.

This is a purpose-built, deterministic skill set kept separate from the real
repo `skills/` (which the `coverage.rs` inventory tests scan) and from
`fixtures/skills/` (the S1 materialize fixture), so growing it never
perturbs those other suites.
