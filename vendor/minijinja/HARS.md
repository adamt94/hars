# minijinja 3.1.0, patched for hars

This is minijinja 3.1.0 from crates.io (Apache-2.0, see `LICENSE`), used
through `[patch.crates-io]` in the workspace `Cargo.toml`.

Why: `render_template` accepts templates from any logged-in user, and each
render must stay inside its fuel budget. minijinja's fuel charges a
container for its own slots only, and its deep walks (printing, comparing)
not at all. So a list that holds one large value many times (`[s] * n`, or
`[b, b]` nested a few levels) cost almost nothing, and an operator that
walks it (`~`, `==`, `in`) then ran for minutes or allocated gigabytes.
`crates/ha-core/src/template/tests.rs` (`hostile_templates_stop_at_the_budget`,
`repeated_and_derived_work_is_paid_for`, `integer_edges_never_panic`,
`short_templates_finish_within_the_budget`) checks each case.

The changes, each marked `hars patch` in the source:

- `src/vm/fuel.rs` `deep_value_cost`, called from `State::charge_value` in
  `src/vm/state.rs`: a value is charged its deep size (every string byte
  and every slot of everything it holds, stopping at the fuel left) when it
  is made (`value_cost`, which this replaces). Values nested deeper than
  256 are refused then (`MAX_VALUE_DEPTH`): the engine's own walks recurse.
- `DeepWalk` does constant work per step (containers are walked through
  iterators, never copied), so a walk cut short stops its work too.
- `src/value/ops.rs` `charge_fuel_for_compare` and `contains`: comparing
  (`==`, `!=`, `<`, …, and each item `in` looks at) charges the deep size of
  the smaller value, walked in step with the larger (`deep_compare_cost`,
  `State::charge_compare`), and two strings the shorter's bytes. A value
  is charged once when it is made, but it can be compared again and again.
- `src/value/ops.rs` `binop_alloc_estimate`: a repeated list (`[x] * n`)
  is sized as `n` times its length, as a repeated tuple already is. The
  repetition stays lazy, but the estimate now refuses it before it is made
  when the fuel can't pay for it, and keeps it out of constant folding,
  which runs at compile time without fuel.
- `src/value/ops.rs` `slice`: a step of `i64::MIN` is read as `-i64::MAX`,
  where negating it overflowed (and panicked).
- `src/compiler/parser.rs`: expression trees are capped at a depth of
  400 while they are built (`MAX_AST_DEPTH`): the parser's recursion plus
  every link of a chain built in a loop (`1+1+…`, `x|f|f…`, `x.a.a…`,
  `not not…`, `- -…`, `a if b else …`, `{% filter f|f… %}`). Unary `not`
  and `-` used to recurse without a guard, and the loops built trees of any
  depth that compiling, constant folding and dropping then recursed
  through, overflowing the stack. Jinja under Python's default recursion
  limit fails at about the same depth.
- `src/filters.rs`: the fueled versions of `reverse`, `items`, `dictsort`,
  `groupby`, `first`, `last` and `length` are public (`#[doc(hidden)]`), so
  hars's wrappers around them keep the fuel accounting.

To update minijinja: copy the new release here, reapply the changes, and
run `cargo test -p ha-core template` (and, optimised,
`cargo test --release -p ha-core short_templates`).
