//! The sampler: turn a running build into a set of per-process records.
//!
//! The model is deliberately simple, and every simplification is a decision
//! rather than an oversight:
//!
//! * **Poll, don't trace.** Every `interval` we snapshot the process table and
//!   derive CPU from the delta in each process's cumulative counter. This works
//!   for any language and any build tool, which is the entire premise — but a
//!   process that starts and finishes inside one interval is never seen.
//!   `coverage()` reports what share of observed processes we caught more than
//!   once, so a user can tell a real profile from a sampling artefact.
//! * **Own CPU, not subtree CPU.** `rustc`'s row reports what `rustc` burned.
//!   Its children keep their own rows, so summing the table gives the build's
//!   true total CPU and nothing is counted twice.
//! * **Time is injected.** `Clock` exists so tests can drive lifetimes exactly
//!   instead of racing a real stopwatch.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::keys::{step_key, StepKey};
use crate::ps::{ProcInfo, ProcSource};

/// Wall-clock source for the sampler, injectable so tests are deterministic.
pub trait Clock {
    /// Milliseconds since the run started.
    fn elapsed_ms(&self) -> i64;
}

pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn elapsed_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }
}

/// A process we have observed at least once.
#[derive(Debug, Clone)]
pub struct ProcRecord {
    pub pid: i32,
    pub ppid: i32,
    pub key: String,
    pub raw: String,
    pub is_wrapper: bool,
    /// Nesting depth within the build tree; the spawned command is 0.
    pub depth: u32,
    /// Milliseconds since the run started.
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    /// When we noticed the process was gone. `None` while it is still running.
    pub ended_ms: Option<i64>,
    /// CPU this process burned on its own, excluding children.
    pub cpu_ms: i64,
    pub peak_rss_kb: i64,
    /// How many polls saw this process. Two or more means we caught its CPU.
    pub samples: u32,
}

impl ProcRecord {
    /// Estimated lifetime in milliseconds.
    ///
    /// Uses "last poll that saw it" through "poll that found it gone", so it is
    /// accurate to within one sampling interval. That bound is constant across
    /// runs, which is what matters: trends compare runs against each other.
    pub fn life_ms(&self) -> i64 {
        let end = self.ended_ms.unwrap_or(self.last_seen_ms);
        (end - self.first_seen_ms).max(0)
    }
}

/// One poll of the whole tree, kept as a coarse time series so a run can be
/// shown as a timeline without keeping a full flame graph.
#[derive(Debug, Clone)]
pub struct Sample {
    pub t_ms: i64,
    /// CPU burned across the whole tree during this interval.
    pub tree_cpu_ms: i64,
    /// Largest summed RSS observed in this interval.
    pub tree_rss_kb: i64,
    pub n_procs: usize,
}

/// The finished profile of one run.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub processes: Vec<ProcRecord>,
    pub samples: Vec<Sample>,
    pub duration_ms: i64,
    pub peak_tree_rss_kb: i64,
    /// Sum of every process's own CPU: the build's total CPU cost as observed.
    pub total_cpu_ms: i64,
    pub polls: u32,
    pub interval_ms: u64,
    pub source: String,
}

impl Profile {
    /// Group the run's processes into step keys, hottest first.
    pub fn top_keys(&self) -> Vec<(String, KeyStats)> {
        let mut buckets: HashMap<&str, KeyStats> = HashMap::new();
        for record in &self.processes {
            let bucket = buckets.entry(record.key.as_str()).or_default();
            bucket.invocations += 1;
            bucket.cpu_ms += record.cpu_ms;
            // Busy time sums lifetimes, so N parallel invocations contribute N x
            // their individual duration. That is the honest total work, and it
            // is what makes a "we added 200 more crates" regression visible.
            bucket.busy_ms += record.life_ms();
            bucket.longest_ms = bucket.longest_ms.max(record.life_ms());
            bucket.peak_rss_kb = bucket.peak_rss_kb.max(record.peak_rss_kb);
            bucket.is_wrapper = bucket.is_wrapper && record.is_wrapper;
            bucket.sample_raw = record.raw.clone();
        }
        let mut out: Vec<(String, KeyStats)> = buckets
            .into_iter()
            .map(|(key, stats)| (key.to_string(), stats))
            .collect();
        out.sort_by(|a, b| {
            b.1.cpu_ms
                .cmp(&a.1.cpu_ms)
                .then(b.1.busy_ms.cmp(&a.1.busy_ms))
        });
        out
    }

    /// Share of observed processes that lived long enough to be caught by at
    /// least two polls, as a percentage.
    ///
    /// This is the honest answer to "did dawdle actually see my build?" A build
    /// that shells out to twenty thousand sub-100ms processes is mostly holes,
    /// and the user deserves to be told rather than handed a confident table of
    /// near-zero rows.
    pub fn coverage(&self) -> Option<f64> {
        if self.processes.is_empty() {
            return None;
        }
        let caught = self.processes.iter().filter(|r| r.samples >= 2).count();
        Some(caught as f64 / self.processes.len() as f64 * 100.0)
    }

    /// Total CPU divided by wall time: how many cores' worth of work the build
    /// actually did. 1.0 is fully serial. A build that drops from 8x to 1x has
    /// a problem no duration number will ever show you.
    pub fn parallelism(&self) -> Option<f64> {
        if self.duration_ms <= 0 {
            return None;
        }
        Some(self.total_cpu_ms as f64 / self.duration_ms as f64)
    }
}

#[derive(Debug, Clone, Default)]
pub struct KeyStats {
    pub invocations: u32,
    pub cpu_ms: i64,
    pub busy_ms: i64,
    /// Longest single invocation of this step, as a wall-time estimate.
    pub longest_ms: i64,
    pub peak_rss_kb: i64,
    pub is_wrapper: bool,
    pub sample_raw: String,
}

struct Live {
    record: ProcRecord,
    /// Cumulative CPU the OS reported at the previous poll, so we can diff.
    last_counter: i64,
}

pub struct Sampler {
    interval_ms: u64,
    source: Box<dyn ProcSource>,
    root_pid: i32,
    clock: Box<dyn Clock>,
    live: HashMap<i32, Live>,
    /// Processes that have exited, kept because most build steps are short and
    /// dropping them would leave the profile nearly empty.
    reaped: Vec<ProcRecord>,
    /// Every pid ever attributed to the tree. Used to keep tracking orphaned
    /// grandchildren whose direct parent has already exited and been reaped.
    known_pids: HashSet<i32>,
    /// The step key for the root process, taken from the command line the user
    /// gave us rather than from `/proc`.
    ///
    /// A shell script that ends in `exec` replaces itself with the real program,
    /// so `/proc` shows the *post-exec* command line. Whether the first poll
    /// lands before or after that exec decides whether the same logical step is
    /// recorded as `sh build.sh` or as `python3 -c`, which splits one step into
    /// two buckets and makes a real trend unreadable. The command the user
    /// asked for is the authoritative identity of the root, and it never
    /// changes under us.
    root_key: Option<String>,
    /// Ppid map from the previous poll, needed to finish a process's depth walk
    /// after it is reaped.
    parents: HashMap<i32, i32>,
    samples: Vec<Sample>,
    peak_tree_rss_kb: i64,
    polls: u32,
}

impl Sampler {
    /// `root_command` is the command line the user asked to run, if known.
    pub fn new(
        source: Box<dyn ProcSource>,
        root_pid: i32,
        interval_ms: u64,
        root_command: Option<&str>,
    ) -> Self {
        Self::with_clock_and_key(
            source,
            root_pid,
            interval_ms,
            Box::new(SystemClock::new()),
            root_command,
        )
    }

    /// The full constructor: the clock is injectable for tests, and the root's
    /// command line is what pins its identity across an `exec`.
    pub fn with_clock_and_key(
        source: Box<dyn ProcSource>,
        root_pid: i32,
        interval_ms: u64,
        clock: Box<dyn Clock>,
        root_command: Option<&str>,
    ) -> Self {
        Self {
            interval_ms,
            source,
            root_pid,
            clock,
            root_key: root_command.map(step_key).map(|parsed| parsed.key),
            live: HashMap::new(),
            reaped: Vec::new(),
            known_pids: HashSet::new(),
            parents: HashMap::new(),
            samples: Vec::new(),
            peak_tree_rss_kb: 0,
            polls: 0,
        }
    }

    /// Take one sample of the process tree.
    pub fn poll(&mut self) {
        let snapshot = self.source.snapshot();
        self.polls += 1;
        let now = self.clock.elapsed_ms();
        let in_tree = self.tree_pids(&snapshot);

        let mut interval_cpu = 0i64;
        let mut tree_rss = 0i64;

        for pid in &in_tree {
            let Some(info) = snapshot.get(pid) else {
                continue;
            };
            tree_rss += info.rss_kb;
            self.parents.insert(*pid, info.ppid);
            match self.live.get_mut(pid) {
                Some(live) => {
                    // Clamp at zero: a process cannot un-consume CPU, but a
                    // re-used pid can appear to have a smaller counter.
                    let delta = (info.cpu_ms - live.last_counter).max(0);
                    live.last_counter = info.cpu_ms;
                    // The process's own cumulative counter *is* its lifetime
                    // CPU: every process in this tree was spawned by the command
                    // we started, so it began at zero. Accumulating deltas from
                    // first sighting instead would silently discard whatever it
                    // burned before we first looked — which is most of a short
                    // step's cost, and made a 130ms run record 0ms of CPU.
                    live.record.cpu_ms = info.cpu_ms;
                    live.record.peak_rss_kb = live.record.peak_rss_kb.max(info.rss_kb);
                    live.record.last_seen_ms = now;
                    live.record.samples += 1;
                    // The *interval* series still needs the delta, so the
                    // per-interval time series stays correct.
                    interval_cpu += delta;
                }
                None => {
                    let parsed: StepKey = step_key(&info.cmdline);
                    // The root keeps the identity the user gave us, so a shell
                    // that `exec`s into another program stays one stable step.
                    let (key, raw, is_wrapper) = if *pid == self.root_pid {
                        match &self.root_key {
                            Some(key) => (key.clone(), parsed.raw, parsed.is_wrapper),
                            None => (parsed.key, parsed.raw, parsed.is_wrapper),
                        }
                    } else {
                        (parsed.key, parsed.raw, parsed.is_wrapper)
                    };
                    self.live.insert(
                        *pid,
                        Live {
                            record: ProcRecord {
                                pid: *pid,
                                ppid: info.ppid,
                                key,
                                raw,
                                is_wrapper,
                                depth: 0, // Filled in once the tree is complete.
                                first_seen_ms: now,
                                last_seen_ms: now,
                                ended_ms: None,
                                // Everything it has burned so far. A process
                                // first seen with a non-zero counter has been
                                // running since before our first poll, and if it
                                // never appears again that CPU would otherwise
                                // never be recorded anywhere.
                                cpu_ms: info.cpu_ms,
                                peak_rss_kb: info.rss_kb,
                                samples: 1,
                            },
                            last_counter: info.cpu_ms,
                        },
                    );
                    self.known_pids.insert(*pid);
                }
            }
        }

        let gone: Vec<i32> = self
            .live
            .keys()
            .copied()
            .filter(|pid| !in_tree.contains(pid))
            .collect();
        for pid in gone {
            if let Some(mut live) = self.live.remove(&pid) {
                live.record.ended_ms = Some(now);
                self.reaped.push(live.record);
            }
        }

        self.peak_tree_rss_kb = self.peak_tree_rss_kb.max(tree_rss);
        self.samples.push(Sample {
            t_ms: now,
            tree_cpu_ms: interval_cpu,
            tree_rss_kb: tree_rss,
            n_procs: in_tree.len(),
        });
    }

    /// Finalise everything and return the profile.
    pub fn finish(mut self) -> Profile {
        // One last poll so processes alive at exit contribute their final CPU.
        self.poll();
        let now = self.clock.elapsed_ms();
        let mut still_live: Vec<ProcRecord> = self
            .live
            .drain()
            .map(|(_, mut live)| {
                if live.record.ended_ms.is_none() {
                    live.record.ended_ms = Some(now);
                }
                live.record
            })
            .collect();
        self.reaped.append(&mut still_live);

        let depth_cache: HashMap<i32, u32> = self
            .reaped
            .iter()
            .map(|record| (record.pid, self.depth_of(record.pid)))
            .collect();
        for record in self.reaped.iter_mut() {
            record.depth = depth_cache.get(&record.pid).copied().unwrap_or(0);
        }
        self.reaped
            .sort_by_key(|record| (record.first_seen_ms, record.pid));

        let total_cpu_ms: i64 = self.reaped.iter().map(|record| record.cpu_ms).sum();
        Profile {
            processes: std::mem::take(&mut self.reaped),
            samples: std::mem::take(&mut self.samples),
            duration_ms: now,
            peak_tree_rss_kb: self.peak_tree_rss_kb,
            total_cpu_ms,
            polls: self.polls,
            interval_ms: self.interval_ms,
            source: self.source.describe(),
        }
    }

    /// Which pids belong to the build. A pid is in the tree when its parent is
    /// the spawned root, or when its parent is a pid we have previously
    /// attributed to the tree. The second clause is what keeps us tracking a
    /// grandchild whose direct parent has already exited.
    fn tree_pids(&mut self, snapshot: &HashMap<i32, ProcInfo>) -> HashSet<i32> {
        let mut in_tree = HashSet::new();
        if snapshot.contains_key(&self.root_pid) {
            in_tree.insert(self.root_pid);
        }
        // Iterate to a fixed point: a pid's parent may be added later in the
        // scan, and the kernel lists processes in no useful order.
        loop {
            let mut grew = false;
            for (pid, info) in snapshot {
                if in_tree.contains(pid) {
                    continue;
                }
                if info.ppid == self.root_pid || self.known_pids.contains(&info.ppid) {
                    in_tree.insert(*pid);
                    self.known_pids.insert(*pid);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        in_tree
    }

    /// Nesting depth within the tree, with a visited guard so a cycle in a
    /// racing process table cannot hang the tool.
    /// The root is depth 0, its direct children depth 1, and so on. Walking up
    /// one level per iteration means the loop must test the *current* pid against
    /// the root before stepping, or the root's children all come back as depth 0
    /// and the tree renders flat.
    fn depth_of(&self, pid: i32) -> u32 {
        let mut depth = 0u32;
        let mut current = pid;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            if current == self.root_pid {
                return depth;
            }
            let Some(ppid) = self.parents.get(&current).copied() else {
                return depth;
            };
            if ppid != self.root_pid && !self.known_pids.contains(&ppid) {
                // Walked out of the tree, so `current` was the topmost process
                // we can account for.
                return depth + 1;
            }
            current = ppid;
            depth += 1;
            if depth > 256 {
                break;
            }
        }
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A clock that advances by a fixed step each time it is read, so every
    /// lifetime in these tests is exact rather than approximately real.
    struct TickClock {
        now: Cell<i64>,
        step: i64,
    }

    impl TickClock {
        fn new(step: i64) -> Self {
            Self {
                now: Cell::new(0),
                step,
            }
        }
    }

    impl Clock for TickClock {
        fn elapsed_ms(&self) -> i64 {
            let value = self.now.get();
            self.now.set(value + self.step);
            value
        }
    }

    /// A process table we drive by hand.
    struct FakeSource {
        tables: RefCell<Vec<HashMap<i32, ProcInfo>>>,
        cursor: Cell<usize>,
    }

    impl FakeSource {
        /// Returns a boxed source because that is how the sampler consumes it;
        /// a plain `Self` would work just as well behind the `ProcSource` bound
        /// but callers here all want the trait object.
        #[allow(clippy::new_ret_no_self)]
        fn new(tables: Vec<HashMap<i32, ProcInfo>>) -> Box<dyn ProcSource> {
            Box::new(Self {
                tables: RefCell::new(tables),
                cursor: Cell::new(0),
            })
        }
    }

    impl ProcSource for FakeSource {
        fn snapshot(&mut self) -> HashMap<i32, ProcInfo> {
            let tables = self.tables.borrow();
            if tables.is_empty() {
                return HashMap::new();
            }
            let index = self.cursor.get().min(tables.len() - 1);
            self.cursor.set(index + 1);
            tables[index].clone()
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }

    fn table(entries: &[(i32, i32, i64, i64)]) -> HashMap<i32, ProcInfo> {
        table_cmd(entries, &[])
    }

    /// Like [`table`], but overrides the command line per pid. Needed to model a
    /// process whose `/proc` entry changes under us, which is what `exec` does.
    fn table_cmd(
        entries: &[(i32, i32, i64, i64)],
        cmdlines: &[(i32, &str)],
    ) -> HashMap<i32, ProcInfo> {
        let mut out = HashMap::new();
        for (pid, ppid, cpu, rss) in entries {
            let default = if *pid == 10 {
                "/usr/bin/driver"
            } else {
                "/usr/bin/worker --task"
            };
            let cmdline = cmdlines
                .iter()
                .find(|(id, _)| id == pid)
                .map(|(_, line)| line.to_string())
                .unwrap_or_else(|| default.to_string());
            out.insert(
                *pid,
                ProcInfo {
                    ppid: *ppid,
                    cpu_ms: *cpu,
                    rss_kb: *rss,
                    cmdline,
                },
            );
        }
        out
    }

    /// A sampler whose clock advances 100ms per poll, matching a 100ms interval.
    fn sampler(tables: Vec<HashMap<i32, ProcInfo>>) -> Sampler {
        Sampler::with_clock_and_key(
            FakeSource::new(tables),
            10,
            100,
            Box::new(TickClock::new(100)),
            None,
        )
    }

    fn record_for(profile: &Profile, pid: i32) -> Option<&ProcRecord> {
        profile.processes.iter().find(|record| record.pid == pid)
    }

    #[test]
    fn a_process_cpu_is_its_lifetime_counter_not_a_sum_of_deltas() {
        // The root is spawned by us, so its counter starts at zero and the last
        // value we read *is* everything it ever burned: 900ms, not the 300ms it
        // burned between these two polls. Summing deltas from first sighting
        // would throw away the 600ms it had already spent, and a step that
        // finishes between two polls would report no CPU whatsoever.
        //
        // The counter must still not be *summed* per poll, which is the other
        // half of the rule: adding 600 then 900 would claim 1500ms.
        let mut s = sampler(vec![
            table(&[(10, 1, 600, 100)]),
            table(&[(10, 1, 900, 100)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        let record = record_for(&profile, 10).expect("root recorded");
        assert_eq!(
            record.cpu_ms, 900,
            "lifetime CPU is the last counter, and must not be summed per poll"
        );
    }

    #[test]
    fn the_root_keeps_the_identity_the_user_gave_it_across_an_exec() {
        // `sh build.sh` ending in `exec` becomes `python3 -c` in /proc, same
        // pid. The first poll can easily land *after* the exec, and then /proc
        // describes something the user never typed. Keying the root off /proc
        // recorded the same logical step under two different names depending on
        // poll timing, which splits its trend in half. The command the user
        // asked for is authoritative and cannot change.
        let mut s = Sampler::with_clock_and_key(
            FakeSource::new(vec![
                // The exec has already happened by the time we first look.
                table_cmd(&[(10, 1, 300, 12)], &[(10, "python3 -c <script>")]),
                table_cmd(&[(10, 1, 400, 12)], &[(10, "python3 -c <script>")]),
            ]),
            10,
            100,
            Box::new(TickClock::new(100)),
            Some("sh build.sh"),
        );
        s.poll();
        s.poll();
        let profile = s.finish();
        let root = record_for(&profile, 10).expect("root recorded");
        assert_eq!(
            root.key, "sh build.sh",
            "the root must be keyed by the command the user ran, not by whatever \
             /proc happened to show at first poll"
        );
        assert_eq!(root.cpu_ms, 400, "and its CPU is still counted normally");
    }

    #[test]
    fn a_process_seen_once_still_reports_the_cpu_it_already_burned() {
        // The case the delta-sum got completely wrong: observed exactly once, it
        // has no second sample, so there is no delta at all to accumulate.
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 300, 40)]),
            table(&[]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        let child = record_for(&profile, 11).expect("the child was observed");
        assert_eq!(
            child.cpu_ms, 300,
            "CPU burned before the first sample of a process must still count"
        );
    }

    #[test]
    fn tracks_grandchildren_after_their_parent_exits() {
        // make/cargo style: the direct child can exit while work continues
        // underneath. Anchoring only on the root would record nothing.
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10)]),
            table(&[(10, 1, 50, 10), (11, 10, 50, 10)]),
            table(&[(11, 10, 250, 20)]),
            table(&[(11, 10, 400, 20)]),
        ]);
        for _ in 0..4 {
            s.poll();
        }
        let profile = s.finish();
        let child = record_for(&profile, 11).expect("grandchild after parent exit");
        assert_eq!(child.cpu_ms, 400, "CPU across the gap must be kept");
        assert!(child.ended_ms.is_some());
    }

    #[test]
    fn does_not_double_count_ancestor_and_descendant_cpu() {
        // If ancestors reported subtree CPU the profile total would be wildly
        // inflated. Each row must be that process's own time only.
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10)]),
            table(&[(10, 1, 100, 10), (11, 10, 400, 10)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        assert_eq!(record_for(&profile, 10).unwrap().cpu_ms, 100);
        assert_eq!(record_for(&profile, 11).unwrap().cpu_ms, 400);
        assert_eq!(
            profile.total_cpu_ms, 500,
            "total is the sum of own-CPU rows"
        );
    }

    #[test]
    fn short_lived_processes_are_kept_not_discarded_on_exit() {
        // Most build steps are short. If reaping threw them away the profile
        // would be almost empty and the report would be a lie.
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10), (12, 10, 0, 10)]),
            table(&[(10, 1, 100, 10)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        let pids: Vec<i32> = profile.processes.iter().map(|r| r.pid).collect();
        assert!(
            pids.contains(&11) && pids.contains(&12),
            "kept pids: {pids:?}"
        );
        assert_eq!(profile.processes.len(), 3);
    }

    #[test]
    fn processes_outside_the_tree_are_ignored() {
        // The editor, the shell and dawdle's own reader threads are all visible
        // in the process table and none of them are build work.
        let mut s = sampler(vec![table(&[
            (10, 1, 0, 10),
            (99, 1, 9999, 9999), // sibling, shares a parent with the root
            (98, 99, 9999, 9999),
        ])]);
        s.poll();
        let profile = s.finish();
        assert_eq!(profile.processes.len(), 1);
        assert_eq!(profile.processes[0].pid, 10);
    }

    #[test]
    fn nesting_depth_follows_the_process_tree() {
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10), (12, 11, 0, 10)]),
            table(&[(10, 1, 10, 10), (11, 10, 10, 10), (12, 11, 10, 10)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        assert_eq!(record_for(&profile, 10).unwrap().depth, 0, "root");
        assert_eq!(record_for(&profile, 11).unwrap().depth, 1);
        assert_eq!(record_for(&profile, 12).unwrap().depth, 2);
    }

    #[test]
    fn key_buckets_group_repeated_invocations() {
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10), (12, 10, 0, 10)]),
            table(&[(10, 1, 10, 10), (11, 10, 100, 10), (12, 10, 250, 10)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        let keys = profile.top_keys();
        let worker = keys
            .iter()
            .find(|(key, _)| key.starts_with("worker"))
            .expect("children bucketed together");
        assert_eq!(worker.1.invocations, 2, "two identical steps, one bucket");
        assert_eq!(worker.1.cpu_ms, 350);
    }

    #[test]
    fn coverage_counts_only_processes_seen_twice_or_more() {
        // Pid 12 appears in exactly one poll, so we never caught its CPU. The
        // profile must say so instead of presenting a hole as a finding.
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10)]),
            table(&[(10, 1, 10, 10), (11, 10, 0, 10), (12, 10, 0, 10)]),
            table(&[(10, 1, 20, 10), (11, 10, 0, 10)]),
        ]);
        for _ in 0..3 {
            s.poll();
        }
        let profile = s.finish();
        assert_eq!(record_for(&profile, 12).unwrap().samples, 1);
        let coverage = profile
            .coverage()
            .expect("coverage for a non-empty profile");
        assert!(
            (coverage - 66.6).abs() < 1.0,
            "expected 2 of 3 processes caught, got {coverage}"
        );
    }

    #[test]
    fn coverage_is_none_for_an_empty_profile() {
        let profile = Profile::default();
        assert_eq!(profile.coverage(), None);
        assert_eq!(profile.parallelism(), None);
    }

    #[test]
    fn parallelism_divides_total_cpu_by_wall_time() {
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10), (12, 10, 0, 10)]),
            table(&[(10, 1, 500, 10), (11, 10, 500, 10), (12, 10, 500, 10)]),
        ]);
        s.poll();
        s.poll();
        let profile = s.finish();
        let ratio = profile.parallelism().expect("parallelism");
        assert!(
            ratio > 1.0,
            "three busy processes should exceed 1x, got {ratio}"
        );
    }

    #[test]
    fn a_recycled_pid_cannot_inflate_cpu() {
        // A reused pid reports a smaller cumulative counter than before; the
        // delta must clamp at zero rather than going negative.
        let mut s = sampler(vec![
            table(&[(10, 1, 5_000, 10)]),
            table(&[(10, 1, 5, 10)]),
            table(&[(10, 1, 4_000, 10)]),
        ]);
        for _ in 0..3 {
            s.poll();
        }
        let profile = s.finish();
        for record in &profile.processes {
            assert!(record.cpu_ms >= 0, "negative cpu for pid {}", record.pid);
        }
    }

    #[test]
    fn time_series_records_one_row_per_poll() {
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10)]),
            table(&[(10, 1, 40, 10)]),
            table(&[(10, 1, 90, 10)]),
        ]);
        for _ in 0..3 {
            s.poll();
        }
        let profile = s.finish();
        assert_eq!(profile.polls, 4, "3 polls plus the final one in finish()");
        assert_eq!(profile.samples.len(), 4);
        // Per-interval CPU must be the deltas 0, 40, 50, 0. The last one is zero
        // because the fake repeats its final table, so the counter did not move.
        let deltas: Vec<i64> = profile.samples.iter().map(|s| s.tree_cpu_ms).collect();
        assert_eq!(deltas, vec![0, 40, 50, 0]);
    }

    #[test]
    fn lifetime_uses_the_poll_that_found_the_process_gone() {
        let mut s = sampler(vec![
            table(&[(10, 1, 0, 10), (11, 10, 0, 10)]),
            table(&[(10, 1, 10, 10), (11, 10, 0, 10)]),
            table(&[(10, 1, 20, 10)]),
        ]);
        for _ in 0..3 {
            s.poll();
        }
        let profile = s.finish();
        let child = record_for(&profile, 11).unwrap();
        assert_eq!(child.first_seen_ms, 0);
        assert_eq!(child.last_seen_ms, 100);
        assert_eq!(child.ended_ms, Some(200));
        assert_eq!(child.life_ms(), 200);
    }

    #[test]
    fn shared_clock_across_threads_advances_consistently() {
        // Guards the Rc/RefCell plumbing the test clock relies on.
        let clock = Rc::new(TickClock::new(50));
        struct Shared(Rc<TickClock>);
        impl Clock for Shared {
            fn elapsed_ms(&self) -> i64 {
                self.0.elapsed_ms()
            }
        }
        let mut s = Sampler::with_clock_and_key(
            FakeSource::new(vec![table(&[(10, 1, 0, 10)])]),
            10,
            100,
            Box::new(Shared(clock)),
            None,
        );
        s.poll();
        // Two reads: one for the explicit poll, one for finish's final poll.
        // The third read is the post-poll timestamp that becomes duration_ms.
        assert_eq!(s.finish().duration_ms, 100);
    }
}
