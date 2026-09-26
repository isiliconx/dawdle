//! Turning a raw process command line into a stable "step key".
//!
//! The whole tool hangs off this function. A build is a forest of short-lived
//! processes, and the only way to answer "which step got slower" across runs is
//! to bucket them into keys that are stable over time. `rustc --crate-name parser`
//! is a key that means the same thing today and next month; `/usr/bin/rustc
//! --crate-name parser --edition=2021 -C debuginfo=2` is not.
//!
//! The design goal is that a key is *short*, *readable*, and *specific enough to
//! act on*. Anything we cannot normalise to that collapses to the bare program
//! name, which is still a useful bucket.

/// Flags that change only verbosity, colour, or parallelism. They never change
/// what work is done, so including them would split one step into N keys.
const NOISE_FLAGS: &[&str] = &[
    "--color",
    "--color=always",
    "--color=never",
    "--colour",
    "--no-color",
    "--no-colour",
    "-v",
    "--verbose",
    "-q",
    "--quiet",
    "-s",
    "--silent",
    "-j",
    "--jobs",
    "--parallel",
    "--frozen",
    "--locked",
    "--offline",
    "-w",
    "--no-print-directory",
    "--print-directory",
    "--print0",
    "-r",
    "--raw",
    "--silent-build",
    "-n",
    "-nt",
    "--dry-run",
    // Toggling features changes what a build compiles but not what kind of work
    // it is. Including it means a local `cargo test` and a CI
    // `cargo test --all-features` land in different buckets, and the report then
    // shows an enormous "regression" that is really just a flag.
    "--all-features",
    "--no-default-features",
    "--features",
];

/// Flags whose *value* identifies the unit of work, so both halves are kept.
const IDENTITY_VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "-C",
    "--crate-name",
    "--profile",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "-Z",
];

/// Single flags that identify the unit of work on their own.
const IDENTITY_FLAGS: &[&str] = &[
    "--lib",
    "--bins",
    "--tests",
    "--benches",
    "--examples",
    "--all-targets",
    "--release",
    "--debug",
];

/// A parsed command key plus the hints the report needs to render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepKey {
    /// The stable bucket, e.g. `cargo test --lib`.
    pub key: String,
    /// Full command line, kept for drill-down.
    pub raw: String,
    /// True when the process is just a wrapper (a shell script, a task runner)
    /// whose own time largely duplicates the work of its children. The report
    /// dims these so `bash build.sh` does not look like the culprit when the
    /// regression is actually in a compiler it invoked.
    pub is_wrapper: bool,
}

/// Split a command line into arguments, honouring single quotes, double quotes
/// and backslash escapes. `/proc` gives us the raw string, not an argv array,
/// so this has to be reasonably faithful before normalisation runs.
pub fn split_args(cmdline: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut chars = cmdline.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\0' => break,
            c if c.is_whitespace() => {
                if has_current {
                    args.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            '\'' => {
                has_current = true;
                for next in chars.by_ref() {
                    if next == '\'' {
                        break;
                    }
                    current.push(next);
                }
            }
            '"' => {
                has_current = true;
                while let Some(next) = chars.next() {
                    if next == '"' {
                        break;
                    }
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            current.push(escaped);
                            continue;
                        }
                    }
                    current.push(next);
                }
            }
            '\\' => {
                has_current = true;
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            _ => {
                has_current = true;
                current.push(c);
            }
        }
    }
    if has_current {
        args.push(current);
    }
    args
}

fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => trimmed.to_string(),
    }
}

fn is_env_assignment(arg: &str) -> bool {
    // `FOO=bar command` — the make/cargo/npx habit of inlining env assignments.
    match arg.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        }
        None => false,
    }
}

fn is_noise_flag(arg: &str) -> bool {
    if NOISE_FLAGS.contains(&arg) {
        return true;
    }
    // Catch prefix forms of the same idea: -j8, -O2 (make), --jobs=4.
    if let Some(rest) = arg.strip_prefix("--jobs=") {
        let _ = rest;
        return true;
    }
    if arg.len() == 3
        && arg.starts_with('-')
        && !arg.starts_with("--")
        && arg[1..].chars().all(|c| c.is_ascii_digit())
        && arg.as_bytes()[1] != b'0'
    {
        return true; // -j8
    }
    false
}

/// Collapse a path-ish argument to something short and stable: keep the
/// basename, but drop build-directory and cache prefixes entirely.
fn shorten_path_arg(arg: &str) -> String {
    let name = basename(arg);
    // Long generated paths (temp dirs, cargo registries, target dirs) are noise.
    if name.len() > 48 {
        return String::new();
    }
    name
}

/// The long spelling of a flag, so that a build invoked with `-p serde` and one
/// invoked with `--package serde` land in the same bucket. Without this, flipping
/// a flag's spelling between a Makefile and a CI script looks like a brand new
/// step appearing and an old one disappearing.
fn canonical_flag(flag: &str) -> &str {
    match flag {
        "-p" => "--package",
        "-C" => "--directory",
        "-m" if flag == "-m" => "-m", // -m means different things per program.
        other => other,
    }
}

fn push_piece(pieces: &mut Vec<String>, piece: String) {
    if piece.is_empty() {
        return;
    }
    if pieces.iter().any(|existing| existing == &piece) {
        return;
    }
    pieces.push(piece);
}

/// The program a command line actually runs.
///
/// `FOO=bar make test` invokes make, not a directory called `bar`, so leading
/// env assignments have to be skipped before the basename is taken.
fn program_of(cmdline: &str) -> String {
    let args = split_args(cmdline);
    let first = args
        .iter()
        .find(|arg| !is_env_assignment(arg))
        .cloned()
        .unwrap_or_default();
    if first.is_empty() {
        return "(unknown)".to_string();
    }
    let name = basename(&first);
    if name.is_empty() {
        first
    } else {
        name
    }
}

/// Build the stable key for a command line.
///
/// The per-program branches exist because the useful identity lives in
/// different argument positions for different ecosystems: for `python3` it is
/// `-m pytest`, for `cargo` it is the subcommand plus target selector, for
/// `make` it is the requested goal.
pub fn step_key(cmdline: &str) -> StepKey {
    let args = split_args(cmdline);
    let is_wrapper = args.first().is_some_and(|first| {
        matches!(
            basename(first).as_str(),
            "sh" | "bash"
                | "zsh"
                | "dash"
                | "env"
                | "timeout"
                | "nice"
                | "script"
                | "just"
                | "task"
                | "tox"
                | "nox"
        )
    });

    if args.is_empty() {
        return StepKey {
            key: "(unknown)".to_string(),
            raw: cmdline.to_string(),
            is_wrapper,
        };
    }

    let program = program_of(cmdline);
    let key = match program.as_str() {
        "rustc" => rustc_key(&args),
        "cargo" => cargo_key(&args),
        "python" | "python3" | "python3.12" | "python3.13" => python_key(&program, &args),
        "node" | "bun" | "deno" => node_key(&program, &args),
        "npm" | "npx" | "pnpm" | "yarn" => package_manager_key(&program, &args),
        "make" | "gmake" => make_key(&args),
        "docker" | "podman" => container_key(&program, &args),
        "cc" | "gcc" | "g++" | "clang" | "clang++" | "ld" | "ld.lld" => {
            compiler_key(&program, &args)
        }
        _ => generic_key(&program, &args),
    };

    StepKey {
        key: truncate_key(&key),
        raw: cmdline.to_string(),
        is_wrapper,
    }
}

fn truncate_key(key: &str) -> String {
    const LIMIT: usize = 72;
    if key.chars().count() <= LIMIT {
        return key.to_string();
    }
    let kept: String = key.chars().take(LIMIT - 1).collect();
    format!("{kept}…")
}

fn rustc_key(args: &[String]) -> String {
    let mut pieces = vec!["rustc".to_string()];
    if let Some(pos) = args.iter().position(|a| a == "--crate-name") {
        if let Some(name) = args.get(pos + 1) {
            push_piece(&mut pieces, format!("--crate-name {name}"));
        }
    }
    if pieces.len() == 1 {
        // No crate name: fall back to the source file so distinct invocations
        // still land in different buckets.
        for arg in args.iter().skip(1) {
            if arg.ends_with(".rs") {
                push_piece(&mut pieces, basename(arg));
                break;
            }
        }
    }
    pieces.join(" ")
}

fn cargo_key(args: &[String]) -> String {
    let mut pieces = vec!["cargo".to_string()];
    let subcommand = args
        .get(1)
        .map(|s| s.as_str())
        .filter(|s| !s.starts_with('-') && !s.contains('=') && !is_env_assignment(s));
    if let Some(sub) = subcommand {
        pieces.push(sub.to_string());
    }
    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if IDENTITY_VALUE_FLAGS.contains(&arg.as_str()) {
            if let Some(value) = args.get(index + 1) {
                if !value.starts_with('-') {
                    push_piece(
                        &mut pieces,
                        format!("{} {}", canonical_flag(arg), shorten_path_arg(value)),
                    );
                }
            }
            index += 2;
            continue;
        }
        if IDENTITY_FLAGS.contains(&arg.as_str()) && !NOISE_FLAGS.contains(&arg.as_str()) {
            push_piece(&mut pieces, arg.clone());
        }
        index += 1;
    }
    pieces.join(" ")
}

fn python_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    // `-m module` or `-c code` is the whole identity. Everything after it is an
    // input to that module — `pytest tests/unit` and `pytest tests/integration`
    // are the same kind of step, and treating the path as identity would make
    // every test directory its own trend.
    let mut entry_taken = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-m" {
            if let Some(module) = args.get(index + 1) {
                push_piece(&mut pieces, format!("-m {module}"));
                entry_taken = true;
            }
            index += 2;
            continue;
        }
        if arg == "-c" {
            // `python3 -c '...'` is always ad-hoc glue; the body is not a stable
            // identity, so collapse it.
            push_piece(&mut pieces, "-c".to_string());
            entry_taken = true;
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            // Step over a value-taking flag's value too, or `-p tsconfig.json`
            // turns the config path into the entry script.
            index += if IDENTITY_VALUE_FLAGS.contains(&arg.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        if !entry_taken {
            let short = shorten_path_arg(arg);
            entry_taken = true;
            push_piece(&mut pieces, short);
        }
        index += 1;
    }
    pieces.join(" ")
}

fn node_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    let mut entry_taken = false;
    let args: Vec<String> = args.to_vec();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if is_noise_flag(arg) {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            // Skip a value-taking flag's value too: in `node .../tsc -p
            // tsconfig.json` the config path is an input, not the entry script.
            index += if IDENTITY_VALUE_FLAGS.contains(&arg.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        // Check the original path: shorten_path_arg has already reduced it to a
        // basename, so a `node_modules/.bin/tsc` check would miss it entirely.
        if arg.contains("node_modules") {
            index += 1;
            continue;
        }
        if entry_taken {
            // Arguments after the entry script are inputs to this run (a config
            // file, a glob), not identity. Keeping them means `next dev` and
            // `next build` with different flags read as different steps.
            index += 1;
            continue;
        }
        let short = shorten_path_arg(arg);
        if short.is_empty() {
            index += 1;
            continue;
        }
        entry_taken = true;
        push_piece(&mut pieces, short);
        index += 1;
    }
    pieces.join(" ")
}

fn package_manager_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    let subcommand = args
        .get(1)
        .map(|s| s.as_str())
        .filter(|s| !s.starts_with('-') && !s.contains('='));
    if let Some(sub) = subcommand {
        pieces.push(sub.to_string());
        // `npm run <script>` — the script name is the entire identity of the step.
        if sub == "run" || sub == "run-script" || sub == "exec" {
            if let Some(script) = args.get(2).filter(|s| !s.starts_with('-')) {
                push_piece(&mut pieces, script.clone());
            }
        }
    }
    pieces.join(" ")
}

/// Compilers get their own branch because every invocation names a different
/// source and object file. Keeping either would split one logical step
/// ("compiling C") into thousands of unique keys, and the count of invocations
/// plus total CPU is the information a reader actually wants.
fn compiler_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    // `-x c++` changes what language is being compiled, which is worth a bucket
    // of its own; every other flag is codegen detail.
    let mut index = 1;
    while index < args.len() {
        if args[index] == "-x" || args[index] == "--x" {
            if let Some(language) = args.get(index + 1) {
                push_piece(&mut pieces, format!("-x {language}"));
            }
            index += 2;
            continue;
        }
        index += 1;
    }
    pieces.join(" ")
}

fn make_key(args: &[String]) -> String {
    let mut pieces = vec!["make".to_string()];
    // `-C dir` is a directory change, not a goal, so its value must be consumed
    // rather than mistaken for one. Among the remaining bare words the goal is
    // the last: `make -C build test` builds `test`, not `build test`.
    let mut goals: Vec<String> = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if is_env_assignment(arg) {
            index += 1;
            continue;
        }
        if arg == "-C" || arg == "--directory" {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        let short = shorten_path_arg(arg);
        if !short.is_empty() {
            goals.push(short);
        }
        index += 1;
    }
    if let Some(goal) = goals.last() {
        push_piece(&mut pieces, goal.clone());
    }
    pieces.join(" ")
}

fn container_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    if let Some(sub) = args.get(1).filter(|s| !s.starts_with('-')) {
        pieces.push(sub.clone());
        if sub == "build" || sub == "compose" || sub == "run" {
            if let Some(target) = args.get(2).filter(|s| !s.starts_with('-')) {
                push_piece(&mut pieces, target.clone());
            }
        }
    }
    pieces.join(" ")
}

/// The fallback: keep the program plus bare (non-flag) arguments, dropping
/// everything we know to be cosmetic.
fn generic_key(program: &str, args: &[String]) -> String {
    let mut pieces = vec![program.to_string()];
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if is_env_assignment(arg) {
            index += 1;
            continue;
        }
        if is_noise_flag(arg) {
            index += 1;
            continue;
        }
        if IDENTITY_VALUE_FLAGS.contains(&arg.as_str()) {
            if let Some(value) = args.get(index + 1) {
                if !value.starts_with('-') && value.len() <= 48 {
                    push_piece(&mut pieces, format!("{arg} {value}"));
                }
            }
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            // Drop `=value` forms entirely: --target-dir=..., -Wl,--as-needed.
            index += 1;
            continue;
        }
        let short = shorten_path_arg(arg);
        push_piece(&mut pieces, short);
        index += 1;
    }
    pieces.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(cmdline: &str) -> String {
        step_key(cmdline).key
    }

    #[test]
    fn splits_quoted_arguments_without_breaking_on_spaces() {
        let args = split_args(r#"node -e "console.log('hi there')" --flag 'single quoted'"#);
        assert_eq!(args[0], "node");
        assert_eq!(args[1], "-e");
        assert_eq!(args[2], "console.log('hi there')");
        assert_eq!(args[3], "--flag");
        assert_eq!(args[4], "single quoted");
    }

    #[test]
    fn rustc_buckets_by_crate_not_by_flag_soup() {
        // The same logical step, invoked with different flag sets, must land in
        // one bucket or a regression becomes invisible.
        let a =
            key_of("/usr/bin/rustc --crate-name parser --edition=2021 -C debuginfo=2 parser.rs");
        let b = key_of("rustc --crate-name parser -C opt-level=3 -C codegen-units=1 parser.rs");
        assert_eq!(a, "rustc --crate-name parser");
        assert_eq!(a, b);
    }

    #[test]
    fn rustc_without_crate_name_still_separates_by_source_file() {
        assert_eq!(key_of("rustc -O2 --edition 2021 /src/a.rs"), "rustc a.rs");
    }

    #[test]
    fn noise_flags_never_split_a_bucket() {
        let quiet = key_of("make -C build --no-print-directory test");
        let loud = key_of("make --color=always -C build -j8 test");
        assert_eq!(quiet, "make test");
        assert_eq!(quiet, loud);
    }

    #[test]
    fn parallel_job_count_does_not_change_the_key() {
        assert_eq!(key_of("cargo build -j 1"), key_of("cargo build -j 64"));
    }

    #[test]
    fn cargo_keeps_subcommand_and_target_selector() {
        assert_eq!(
            key_of("cargo test --lib --all-features"),
            "cargo test --lib"
        );
        assert_eq!(
            key_of("cargo build -p serde_json --release"),
            "cargo build --package serde_json --release"
        );
    }

    #[test]
    fn cargo_flags_do_not_leak_into_the_key() {
        let key = key_of("cargo build --target-dir /very/long/target/dir --offline -v");
        assert_eq!(key, "cargo build");
    }

    #[test]
    fn python_module_is_the_identity() {
        assert_eq!(
            key_of("python3 -m pytest tests/integration -q --tb=short"),
            "python3 -m pytest"
        );
        assert_eq!(
            key_of("/usr/bin/python3.12 -m compileall -q src"),
            "python3.12 -m compileall"
        );
    }

    #[test]
    fn node_entry_script_is_the_identity() {
        assert_eq!(
            key_of("node /repo/scripts/build.js --watch"),
            "node build.js"
        );
    }

    #[test]
    fn node_modules_requires_are_not_identity() {
        assert_eq!(
            key_of("node /repo/node_modules/.bin/tsc -p tsconfig.json"),
            "node"
        );
    }

    #[test]
    fn npm_run_script_is_the_identity() {
        assert_eq!(key_of("npm run build"), "npm run build");
        assert_eq!(key_of("npm run test:unit -- --watch"), "npm run test:unit");
    }

    #[test]
    fn shell_wrappers_are_flagged_so_they_can_be_dimmed() {
        let parsed = step_key("bash scripts/build.sh");
        assert!(parsed.is_wrapper);
        assert_eq!(parsed.key, "bash build.sh");
        assert!(!step_key("rustc --crate-name parser").is_wrapper);
    }

    #[test]
    fn inline_env_assignments_are_dropped() {
        assert_eq!(key_of("PATH=/opt/bin:/usr/bin make test"), "make test");
    }

    #[test]
    fn long_generated_paths_are_dropped() {
        let key = key_of("cc -c /tmp/some/really/deep/target/release/build/foo-1234/out/thing.o");
        assert_eq!(key, "cc");
    }

    #[test]
    fn keys_stay_short_even_for_argument_spew() {
        let key = key_of(
            "node script.js aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff",
        );
        assert!(key.chars().count() <= 72, "key too long: {key}");
    }

    #[test]
    fn unknown_program_still_gets_a_bare_key() {
        assert_eq!(
            key_of("/opt/vendor/bin/weirdtool --alpha --beta"),
            "weirdtool"
        );
    }

    #[test]
    fn empty_commandline_is_safe() {
        assert_eq!(key_of(""), "(unknown)");
    }
}
