# minijinja 3.1.0, patched for hars

This is minijinja 3.1.0 from crates.io (Apache-2.0, see `LICENSE`), used
through `[patch.crates-io]` in the workspace `Cargo.toml`. One change:

- `src/vm/fuel.rs` `deep_value_cost`, called from `State::charge_value` in
  `src/vm/state.rs`: fuel charges a value's deep size (every string byte
  and every slot of everything it holds, stopping at the fuel left) when
  the value is made, where minijinja charges a container's own slots only
  (`value_cost`, which this replaces).
- `src/value/ops.rs` `binop_alloc_estimate`: a repeated list (`[x] * n`)
  is sized as `n` times its length, as a repeated tuple already is. The
  repetition stays lazy, but the estimate now refuses it before it is made
  when the fuel can't pay for it, and keeps it out of constant folding,
  which runs at compile time without fuel (`[1] * 100000000 == …` folded
  would compare a hundred million items while compiling).

Why: `render_template` accepts templates from any logged-in user. Without
the change, a list that holds one large value many times (`[s] * n`, or
`[b, b]` nested a few levels) costs almost nothing, and an operator that
walks it deeply (`~`, `==`, `in`) then runs for minutes or allocates
gigabytes outside the budget. `crates/ha-core/src/template/tests.rs`
(`hostile_templates_stop_at_the_budget`) checks each case.

To update minijinja: copy the new release here, reapply the change, and run
`cargo test -p ha-core template`.
