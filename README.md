# dawdle

**Find the step that made your build slow, and the commit that did it.**

`dawdle` records every build you run into a local SQLite database, then tells you
which step got slower, when it started, and which commits are responsible. A
flamegraph shows you where one run went. `dawdle` remembers every run, so it can
show you the one that changed.

```
$ dawdle report

12 runs · 2026-09-26 → 2026-09-26 · just now · metric busy

  run total 235ms → 1.54s  +557%  since run #7 (8a0e700)  over 12 runs

REGRESSIONS
  step             trend         before  now   change  n  peak rss  since
  python3 slow.py  ▁▂▂▂▂▂▇▇▇▇█▇   211ms  1.49s  +604%   1      14MB  8a0e700 just now
  make test        ▂▂▂▂▂▂▇▇▇▇█▇   228ms  1.53s  +570%   1       3MB  8a0e700 just now
  next: dawdle blame "python3 slow.py"
```

```
$ dawdle blame "python3 slow.py"

python3 slow.py — 211ms → 1.49s
  metric busy · 12 runs · ▁▂▂▂▂▂▇▇▇▇█▇

  first crossed the threshold at run #7 on 2026-09-26, just now

  commit range 960dcb2..8a0e700

commit   when        author  subject
8a0e700  2026-09-26  you     test step: 400k iterations to 3M

  bisect: git bisect start 8a0e700 960dcb2
```

The bold block in the sparkline is the run where the step first crossed the
threshold, so you can see the regression rather than infer it.

## Install

```sh
cargo install --git https://github.com/isiliconx/dawdle
```

Or build from source — there is no build system beyond `cargo build --release`,
and the result is a single static binary:

```sh
git clone https://github.com/isiliconx/dawdle
cd dawdle && cargo build --release
./target/release/dawdle --help
```

Rust 1.83 or newer. No system `libsqlite3` needed: SQLite is compiled in.

## Use

Wrap the build. That is the whole setup.

```sh
dawdle run --label ci -- make test
```

Every subsequent `dawdle run` with that label is compared against the ones
before it, so the second week you use it starts producing answers:

```sh
dawdle report              # what changed
dawdle blame "cargo test"  # which commits
dawdle show 42             # everything about one run
dawdle ls                  # recent runs
```

In CI, `dawdle run` prints a verdict line on its own, and `--markdown` gives a
table ready to paste into a PR comment:

```sh
dawdle run --label ci -- make test
dawdle report --markdown >> "$GITHUB_STEP_SUMMARY"
```

Profiles live in `./.dawdle/profile.db`, one per repository, and dawdle adds
that directory to `.git/info/exclude` itself so it never touches your
`.gitignore`. `dawdle prune --keep 100` trims history.

## What it measures

`dawdle run` samples the process tree of the command you gave it, on Linux via
`/proc` and on macOS via `ps`, every 20ms. It records, per process: CPU time,
peak RSS, first and last sighting, parent, and full command line. On exit it
rolls those up three ways:

- **busy** — summed process lifetime, wall-clock occupancy. The default metric,
  because "this step got slower" is a statement about duration.
- **cpu** — CPU actually burned. Comparable across machines, unlike wall time on
  a shared CI box.
- **longest** — wall time of the slowest single invocation.

Command lines are normalised into stable *step keys* so the same logical work
lands in the same bucket across runs. `make -C build test` is `make test`;
`cargo test --lib --all-features` and `cargo test --lib` are both
`cargo test --lib`; a compiler invoked a thousand times with different filenames
is one `cc` step, not a thousand.

## Calling something a regression

Both thresholds have to trip:

- `--threshold`, default 25% — relative change between the older and newer halves
  of the window.
- `--min-ms`, default 100 — absolute change. Without it, a 4ms step tripling is a
  200% "regression" and drowns out a real one.

Comparison is median-based, so one slow run does not move the baseline. Steps are
also shown next to their *invocation count*: a step that is 3x slower because it
now runs 3x as often is a different problem from one that got 3x slower per
invocation, and the two columns are the only way to tell them apart.

## Honest limitations

- **Sampling has a floor.** dawdle polls, so a process that lives entirely
  between two samples is invisible. The interval is the shortest step dawdle can
  possibly see, which is why the default is 20ms and not the more innocent-looking
  100ms. `dawdle show` reports sampling *coverage* — the share of processes seen
  at least twice — so you can tell when a build is made mostly of steps too short
  to catch. Catching every process needs eBPF or ptrace, which is a different tool
  with different privileges.
- **macOS numbers are approximate.** `ps` reports wall-clock elapsed, not CPU.
  Linux reads real CPU counters from `/proc`.
- **CPU is read at the kernel's tick rate** (USER_HZ, 100 on nearly every Linux
  system, asked from `sysconf` rather than assumed). A process shorter than one
  tick can still be undercounted.
- **History is per repository and local.** Nothing leaves the machine and nothing
  is uploaded. There is no cross-machine comparison yet, so a CI runner replaced
  every week starts from nothing.
- **Blame needs git.** Outside a repository dawdle still records and reports
  durations; it just cannot name commits, and says so rather than inventing a
  range.

## How it works

Polling the process tree is a deliberate choice over eBPF or ptrace: it needs no
privileges, works on macOS too, and degrades to something honest rather than
something that silently requires root. The cost is the sampling floor above.

A run becomes a row in `runs`, its processes become rows in `processes`, and a
per-step rollup lands in `step_totals`. The report only ever reads `step_totals`,
so report cost is independent of how much detail a run recorded. Onset detection
compares a step against its own history rather than bisecting, so a step that has
always been slow is never blamed for getting slower.

Around 6,400 lines of Rust and four runtime dependencies: `anyhow`, `rusqlite`
(bundled), `serde`, `serde_json`. The argument parser is hand-rolled to keep the
dependency count and the binary down. 95 tests, including ones that pin the CPU
accounting, key normalisation and onset detection against deliberately wrong
input.

## Licence

MIT. See [LICENSE](LICENSE).
