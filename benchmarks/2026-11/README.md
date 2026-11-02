# Herdr vs musher, November 2026

`bench.py <herdr|musher> <A|B|C|D> <trial> [musher-binary]`, run from a folder holding `base-a`, `base-b`, `base-c` as git repos (edit `B` at the top).

- A: three independent functions in three files.
- B: all three go in one file, so the branches conflict. Herdr: `git merge`, then one extra `claude -p` per conflict. musher: `merge --resolve`.
- C: project rules (JSDoc, RangeError only) live in CONTRIBUTING.md and `scripts/ci.mjs`, not in the prompt. Both get the same allow list. A permission prompt is answered at once and counted.
- D: `echo run-N && npm test` under `acceptEdits`. Time until the tool reports it needs a person, then approve.

Cost: `summarize.py` reads session logs with one formula (Opus 5.5 $4/$20 per MTok, cache read 0.4, write 5, 1h write 8); `resolve_cost` is added for Herdr in B. `results.jsonl` holds every counted trial. Two earlier Herdr batches were thrown out because of bugs in this harness (invalid agent name, CI command missing from the allow list), not because of Herdr.

The bench answers Herdr prompts instantly. A person would be slower, on both tools.
