//! What `giverny manage` learns its estimates from.
//!
//! Every task that lands appends one line to `history.jsonl` beside the
//! feeds: its estimate (the guess as given), its
//! wall-clock time, and its *working* time — wall time minus the spans it was
//! paused, the spans its worker said it was waiting and the spans the
//! machine slept through ([`crate::sleeps`]). Append-only, one JSON
//! object per line, so concurrent manager sessions never lose each other's lines and a
//! line this version cannot read is skipped, not fatal.
//!
//! Nothing is corrected: every figure goes on the pane and into the history
//! as given. Each estimator is scored on its own **track** — the
//! dispatcher's guess against the whole working time, and the worker's first
//! re-estimate (made after reading the code) against the working time that
//! was still to come when it was made — and is *told* how far off its gut
//! figure runs ([`track_record`]): the median of working time ÷ its *gut*
//! figure over the most recent landed tasks of the same kind (the same repo
//! and the same type word in the title, `BUG:`, `FEATURE:` …, else the same
//! repo, else every task). Plain arithmetic over the file, nothing else.
//! [`accuracy`] reports every track's error, older against recent, so
//! whether estimates improve is read from the data.
//!
//! **The gut, not the figure**. Scoring the figures as
//! given undoes itself: once givers allow for the factor and land on
//! target, the records come in at ×1.0, the factor drifts to ×1.0, the
//! givers go back to their gut and it swings back. So each figure is kept
//! with the giver's gut figure, its raw figure before allowing for how far
//! off it usually runs (`gut_s`, `reest_gut_s`: `--gut`, else the figure
//! itself), and the factor told is `median(work ÷ gut)`. It stays for as
//! long as the gut is off, and follows it to ×1.0 only when the gut itself
//! improves; how much of it a giver allows for is the giver's judgement.
//!
//! **Lines from older versions** carry no gut. A dispatcher then
//! scaled its gut by the factor it had been shown (`told_x`), as a rule, so
//! its gut is worked back out as `given ÷ told_x`. A worker was told its
//! factor as information and mostly gave its gut: assuming it applied the
//! factor read each figure as a gut ×factor too low, the factor told fell
//! further, and the track ran away toward ×0.01. Its gut is the figure as
//! given. And the factor told is held to [`X_MIN`]–[`X_MAX`], a band a
//! giver can believe, should anything run away again.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The history's file name inside the feed directory. Not `*.json`, so the
/// feed scan ([`crate::feed::find`]) never reads it as a feed.
pub const FILE: &str = "history.jsonl";

/// Overrides where the history lives (an empty value turns learning off).
pub const ENV: &str = "GIVERNY_MANAGE_HISTORY";

/// How many matching tasks a level needs before its track record is told.
pub const MIN_SAMPLES: usize = 5;
/// How many of the most recent matching tasks the median is taken over.
pub const RECENT: usize = 20;

/// The band the factor told is held to: outside it, a factor
/// no giver would believe.
pub const X_MIN: f64 = 0.2;
/// See [`X_MIN`].
pub const X_MAX: f64 = 3.0;

/// One landed task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The guess as given to `plan`/`start --eta`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_s: Option<u64>,
    /// The dispatcher's gut figure behind it (`--gut`, else the guess
    /// itself). `None` on lines written before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gut_s: Option<u64>,
    /// What the pane counted down from at the start: the guess itself, or
    /// on lines an older version wrote, the guess corrected. Kept so old
    /// lines read; no track scores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_s: Option<u64>,
    /// The last estimate, after any re-estimates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_final_s: Option<u64>,
    /// The factor the dispatcher had been shown for its guess track when it
    /// gave `estimate_s`: ×1 when none was shown, and on
    /// lines written before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub told_x: Option<f64>,
    /// The worker's first re-estimate of the time left, as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reest_s: Option<u64>,
    /// The worker's gut figure behind it, as [`Record::gut_s`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reest_gut_s: Option<u64>,
    /// The factor the worker had been shown for its re-estimate track when
    /// it gave `reest_s`, as [`Record::told_x`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reest_told_x: Option<f64>,
    /// Working time already spent when the re-estimate was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reest_at_s: Option<u64>,
    pub wall_s: u64,
    #[serde(default)]
    pub paused_s: u64,
    #[serde(default)]
    pub wait_s: u64,
    /// What the machine slept through between start and end.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub slept_s: u64,
    pub work_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The highest peak memory of the task's `giverny manage run` commands,
    /// MiB: the cgroup's `memory.peak` under a systemd
    /// scope, else the largest process's RSS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_mb: Option<u64>,
    /// CPU time of those commands, summed, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_s: Option<u64>,
    /// How many of them the memory cap killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_kills: Option<u64>,
}

impl Record {
    /// Did the work finish (`Done`, `Review`)? A blocked or cancelled task
    /// stopped early and teaches nothing about estimates.
    fn finished(&self) -> bool {
        self.outcome
            .as_deref()
            .is_none_or(|o| o.starts_with("Done") || o.starts_with("Review"))
    }
}

/// One estimator's figures, each scored against what it estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// The dispatcher's guess (`plan`/`start --eta`), as given, against the
    /// whole working time.
    Guess,
    /// The worker's first re-estimate of the time left, as given, against
    /// the working time still to come when it was made.
    Reestimate,
}

impl Track {
    pub const ALL: [Track; 2] = [Track::Guess, Track::Reestimate];

    /// `(estimated, took)` in seconds, when `r` teaches this track anything.
    pub fn pair(self, r: &Record) -> Option<(u64, u64)> {
        if !r.finished() || r.work_s == 0 {
            return None;
        }
        let left = || r.work_s.checked_sub(r.reest_at_s?).filter(|l| *l > 0);
        let (est, took) = match self {
            Track::Guess => (r.estimate_s?, r.work_s),
            Track::Reestimate => (r.reest_s?, left()?),
        };
        (est > 0).then_some((est, took))
    }

    /// The factor the giver had been shown when giving this track's figure
    /// on `r`: ×1 when none was (or the line is an older version's), and
    /// for anything that is not a usable factor.
    pub fn told_x(self, r: &Record) -> f64 {
        let x = match self {
            Track::Guess => r.told_x,
            Track::Reestimate => r.reest_told_x,
        };
        x.filter(|x| x.is_finite() && *x > 0.0).unwrap_or(1.0)
    }

    /// The giver's gut figure on `r`, seconds: `--gut` (on lines from
    /// this version, the figure itself when none was given); on older ones, a
    /// dispatcher's figure as given ÷ the factor it had been shown and a
    /// worker's figure as given (see the module's doc).
    pub fn gut(self, r: &Record) -> Option<f64> {
        let (est, _) = self.pair(r)?;
        let gut = match self {
            Track::Guess => r.gut_s.map(|g| g as f64),
            Track::Reestimate => r.reest_gut_s.map(|g| g as f64),
        };
        let gut = gut.unwrap_or(match self {
            Track::Guess => est as f64 / self.told_x(r),
            Track::Reestimate => est as f64,
        });
        (gut > 0.0).then_some(gut)
    }

    /// How the giver's *gut* ran on `r`: took ÷ the gut figure. What the
    /// giver is told ([`track_record`]).
    pub fn ratio(self, r: &Record) -> Option<f64> {
        let (_, took) = self.pair(r)?;
        Some(took as f64 / self.gut(r)?)
    }

    /// How the figure *as given* ran on `r`: took ÷ estimated. Near ×1.0
    /// when the factor told is being applied.
    pub fn given_ratio(self, r: &Record) -> Option<f64> {
        self.pair(r).map(|(e, t)| t as f64 / e as f64)
    }

    /// What the track's estimates are called: `BUG guesses`.
    pub fn noun(self) -> &'static str {
        match self {
            Track::Guess => "guesses",
            Track::Reestimate => "re-estimates",
        }
    }

    /// The heading [`accuracy`] gives it.
    pub fn heading(self) -> &'static str {
        match self {
            Track::Guess => "dispatcher's guess, as given",
            Track::Reestimate => "worker's re-estimate of the time left, as given",
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Where the history lives: `$GIVERNY_MANAGE_HISTORY`, else
/// `<feed dir>/history.jsonl`. `None` when the variable is set but empty.
pub fn path(feed_dir: &Path) -> Option<PathBuf> {
    match std::env::var_os(ENV) {
        Some(v) if v.is_empty() => None,
        Some(v) => Some(PathBuf::from(v)),
        None => Some(feed_dir.join(FILE)),
    }
}

/// Append one record. A single `write` of a whole line on a file opened for
/// appending, so concurrent writers interleave lines, never bytes.
pub fn append(file: &Path, rec: &Record) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(rec)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(&line)
}

/// Every record that parses, oldest first.
pub fn load(file: &Path) -> Vec<Record> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The type word a title starts with: `BUG: x` → `BUG`. An upper-case word
/// of two or more letters followed by a colon; anything else has no kind.
pub fn kind_of(title: &str) -> Option<String> {
    let (head, _) = title.trim_start().split_once(':')?;
    let ok = head.len() >= 2 && head.len() <= 16 && head.chars().all(|c| c.is_ascii_uppercase());
    ok.then(|| head.to_string())
}

/// The repo a task is from: `<repo>#<n>` in its key names it; otherwise the
/// repository the command runs in (`cwd`), a worktree counting as its main
/// checkout; otherwise `cwd`'s own name.
pub fn repo_of(key: &str, cwd: &Path) -> Option<String> {
    if let Some((name, n)) = key.split_once('#') {
        let name = name.rsplit('/').next().unwrap_or(name);
        if !name.is_empty() && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return Some(name.to_string());
        }
    }
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return name_of(dir);
        }
        if git.is_file() {
            // A worktree: `gitdir: <main>/.git/worktrees/<name>`.
            let main = std::fs::read_to_string(&git).ok().and_then(|s| {
                let gd = s.trim().strip_prefix("gitdir:")?.trim().to_string();
                let cut = gd.find("/.git/")?;
                Some(PathBuf::from(&gd[..cut]))
            });
            return main.as_deref().and_then(name_of).or_else(|| name_of(dir));
        }
    }
    name_of(cwd)
}

fn name_of(dir: &Path) -> Option<String> {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The most recent ratios of `track` at the narrowest level with
/// [`MIN_SAMPLES`] (repo and kind, repo, all), with that level's words.
fn level_ratios(
    history: &[Record],
    track: Track,
    repo: Option<&str>,
    kind: Option<&str>,
) -> Option<(Vec<f64>, String)> {
    let levels: [(Option<&str>, Option<&str>); 3] = [(repo, kind), (repo, None), (None, None)];
    for (i, (r, k)) in levels.into_iter().enumerate() {
        // Skip a level that would only repeat the next one.
        if (i == 0 && (r.is_none() || k.is_none())) || (i == 1 && r.is_none()) {
            continue;
        }
        let ratios: Vec<f64> = history
            .iter()
            .rev()
            .filter(|h| r.is_none_or(|r| h.repo.as_deref() == Some(r)))
            .filter(|h| k.is_none_or(|k| h.kind.as_deref() == Some(k)))
            .filter_map(|h| track.ratio(h))
            .take(RECENT)
            .collect();
        if ratios.len() < MIN_SAMPLES {
            continue;
        }
        // What the gut is on: ` on FEATURE tasks in demo`, ` in demo`, ``.
        let words = match (r, k) {
            (Some(r), Some(k)) => format!(" on {k} tasks in {r}"),
            (Some(r), None) => format!(" in {r}"),
            _ => String::new(),
        };
        return Some((ratios, words));
    }
    None
}

/// How an estimator's gut runs on tasks like this one:
/// the factor it is told (`x`, held to [`X_MIN`]–[`X_MAX`]; `measured` is
/// the median itself), how many tasks it is the median of, and the level
/// it was found at (` on BUG tasks in giverny`, ` in giverny` or empty),
/// which is also what tells one line from another. `None` with too little
/// history.
#[derive(Debug, Clone, PartialEq)]
pub struct Instinct {
    pub track: Track,
    pub x: f64,
    pub measured: f64,
    pub n: usize,
    pub level: String,
}

impl Instinct {
    /// How far off the gut runs, in words a reader cannot turn around:
    /// `about 3.0× too high`, `about 1.5× too low`, `about right`; a
    /// factor held to the band says `or more`.
    pub fn how_far(&self) -> String {
        let held = if self.measured == self.x {
            ""
        } else {
            " or more"
        };
        if self.x < 1.0 / 1.15 {
            format!("about {:.1}× too high{held}", 1.0 / self.x)
        } else if self.x > 1.15 {
            format!("about {:.1}× too low{held}", self.x)
        } else {
            "about right".to_string()
        }
    }

    /// The line its giver reads: `your gut re-estimates on BUG tasks in
    /// giverny have usually run about 3.0× too high (last 20, median)`.
    pub fn line(&self) -> String {
        let what = match self.track {
            Track::Guess => "estimates",
            Track::Reestimate => "re-estimates",
        };
        let verb = if self.how_far() == "about right" {
            "been"
        } else {
            "run"
        };
        format!(
            "your gut {what}{} have usually {verb} {} (last {}, median)",
            self.level,
            self.how_far(),
            self.n
        )
    }
}

/// What to give after the line ([`Instinct::line`]): the gut figure and the
/// one settled on, the figure the pane shows. How much to
/// allow for the line is the giver's judgement.
pub const GIVE_BOTH: &str = "give your gut figure and the one you settle on";

/// The gut factor of `track` for a task of this repo and kind: the median
/// of working time ÷ gut figure over the most recent finished tasks at the
/// narrowest level with [`MIN_SAMPLES`], held to [`X_MIN`]–[`X_MAX`].
pub fn instinct(
    history: &[Record],
    track: Track,
    repo: Option<&str>,
    kind: Option<&str>,
) -> Option<Instinct> {
    let (ratios, level) = level_ratios(history, track, repo, kind)?;
    let measured = median(ratios.clone());
    Some(Instinct {
        track,
        x: measured.clamp(X_MIN, X_MAX),
        measured,
        n: ratios.len(),
        level,
    })
}

/// How an estimator's gut runs, for it to see before it makes the next
/// figure: [`Instinct::line`]. Told, never applied: the figure it settles
/// on is the figure the pane shows. `None` with too little history.
pub fn track_record(
    history: &[Record],
    track: Track,
    repo: Option<&str>,
    kind: Option<&str>,
) -> Option<String> {
    instinct(history, track, repo, kind).map(|i| i.line())
}

/// One group's figures on a track: how many, the median gut ratio (the
/// factor its givers are told), the median ratio of the
/// figures as given (bias: near ×1.0 when the factor is allowed for) and their
/// typical miss either way (the median of `max(r, 1/r)`: ×1.5 means half the
/// figures were within half again of the truth).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub n: usize,
    pub instinct: f64,
    pub bias: f64,
    pub off: f64,
}

/// [`Score`] of a set of `(instinct, as given)` ratios, `None` when empty.
pub fn score(ratios: &[(f64, f64)]) -> Option<Score> {
    if ratios.is_empty() {
        return None;
    }
    Some(Score {
        n: ratios.len(),
        instinct: median(ratios.iter().map(|r| r.0).collect()),
        bias: median(ratios.iter().map(|r| r.1).collect()),
        off: median(ratios.iter().map(|(_, r)| r.max(1.0 / r)).collect()),
    })
}

/// `giverny manage accuracy`: every track's error, older half
/// against recent half, for all tasks, each repo, and each repo's types with
/// [`MIN_SAMPLES`] or more. `repo` narrows it to one repo. Each group shows
/// both the instinct factor and how the figures as given landed.
pub fn accuracy(history: &[Record], repo: Option<&str>) -> String {
    let history: Vec<&Record> = history
        .iter()
        .filter(|h| repo.is_none_or(|r| h.repo.as_deref() == Some(r)))
        .collect();
    let mut out = format!(
        "Estimate accuracy over {} landed task{}{}.\n\
         instinct = time worked ÷ the gut figure (median; `--gut`, else the figure as \
         given, and on a dispatcher's guess from an older version the figure as given \
         ÷ the factor shown): the factor the line tells; ×1.00 is a gut that is right.\n\
         given = time worked ÷ the figure as given (median): near ×1.00 when the line \
         is allowed for, drifting from it when it is not.\n\
         off = the typical miss of the figures as given, either way (median of the \
         ratio or its inverse): ×1.00 is perfect.\n\
         older / recent = the first and second half of each group, oldest first.\n",
        history.len(),
        if history.len() == 1 { "" } else { "s" },
        repo.map(|r| format!(" in {r}")).unwrap_or_default()
    );
    let mut repos: Vec<&str> = history.iter().filter_map(|h| h.repo.as_deref()).collect();
    repos.sort_unstable();
    repos.dedup();
    let fmt = |s: Option<Score>| match s {
        Some(s) => format!(
            "{:>3}  ×{:<7.2} ×{:<5.2} ×{:<5.2}",
            s.n, s.instinct, s.bias, s.off
        ),
        None => format!("{:>3}  {:<8} {:<6} {:<6}", 0, "—", "—", "—"),
    };
    for track in Track::ALL {
        // Each half's column names sit over its figures.
        let cols = "  n  instinct given  off";
        out.push_str(&format!(
            "\n{}\n  {:<24}{:<34}{:<34}\n  {:<24}{cols:<34}{cols:<34}\n",
            track.heading(),
            "",
            "older",
            "recent",
            "",
        ));
        let mut groups: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
        let ratios = |f: &dyn Fn(&Record) -> bool| -> Vec<(f64, f64)> {
            history
                .iter()
                .filter(|h| f(h))
                .filter_map(|h| Some((track.ratio(h)?, track.given_ratio(h)?)))
                .collect()
        };
        let all = ratios(&|_| true);
        if all.is_empty() {
            out.push_str("  no tasks with this figure yet\n");
            continue;
        }
        if repo.is_none() {
            groups.push(("all".into(), all));
        }
        for r in &repos {
            groups.push(((*r).to_string(), ratios(&|h| h.repo.as_deref() == Some(r))));
            let mut kinds: Vec<&str> = history
                .iter()
                .filter(|h| h.repo.as_deref() == Some(r))
                .filter_map(|h| h.kind.as_deref())
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            for k in kinds {
                let v = ratios(&|h| h.repo.as_deref() == Some(r) && h.kind.as_deref() == Some(k));
                if v.len() >= MIN_SAMPLES {
                    groups.push((format!("{r} {k}"), v));
                }
            }
        }
        for (name, v) in groups.into_iter().filter(|(_, v)| !v.is_empty()) {
            let (old, new) = v.split_at(v.len() / 2);
            out.push_str(&format!(
                "  {:<24}{:<34}{:<34}\n",
                name,
                fmt(score(old)),
                fmt(score(new))
            ));
        }
    }
    out.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// How many tasks with a measured peak a level needs before
/// [`peak_hint`] speaks.
pub const MIN_PEAK_SAMPLES: usize = 3;

/// The measured peaks of the most recent tasks like this one, by the same
/// levels as [`track_record`] (repo and kind, repo, all): the narrowest
/// with [`MIN_PEAK_SAMPLES`], newest first, with how to name the level
/// (`BUG tasks in demo`).
fn peak_sample(
    history: &[Record],
    repo: Option<&str>,
    kind: Option<&str>,
) -> Option<(String, Vec<u64>)> {
    sample(history, repo, kind, &|h| h.peak_mb.filter(|p| *p > 0))
}

/// One figure (`of`) of the most recent tasks like this one that have it,
/// by the levels [`peak_sample`] uses.
fn sample(
    history: &[Record],
    repo: Option<&str>,
    kind: Option<&str>,
    of: &dyn Fn(&Record) -> Option<u64>,
) -> Option<(String, Vec<u64>)> {
    let levels: [(Option<&str>, Option<&str>); 3] = [(repo, kind), (repo, None), (None, None)];
    for (i, (r, k)) in levels.into_iter().enumerate() {
        if (i == 0 && (r.is_none() || k.is_none())) || (i == 1 && r.is_none()) {
            continue;
        }
        let peaks: Vec<u64> = history
            .iter()
            .rev()
            .filter(|h| r.is_none_or(|r| h.repo.as_deref() == Some(r)))
            .filter(|h| k.is_none_or(|k| h.kind.as_deref() == Some(k)))
            .filter_map(of)
            .take(RECENT)
            .collect();
        if peaks.len() < MIN_PEAK_SAMPLES {
            continue;
        }
        let what = match (r, k) {
            (Some(r), Some(k)) => format!("{k} tasks in {r}"),
            (Some(r), None) => format!("tasks in {r}"),
            _ => "tasks".to_string(),
        };
        return Some((what, peaks));
    }
    None
}

/// What the most recent tasks like this one peaked at under `giverny manage
/// run`, by the same levels as [`track_record`] (repo and kind, repo, all):
/// `the last 4 BUG tasks in demo peaked at 1.8G (median), 2.6G at most`.
/// A task the cap killed counts at its peak, which is a floor.
pub fn peak_hint(history: &[Record], repo: Option<&str>, kind: Option<&str>) -> Option<String> {
    let (what, peaks) = peak_sample(history, repo, kind)?;
    let max = peaks.iter().copied().max().unwrap_or(0);
    let med = median(peaks.iter().map(|p| *p as f64).collect()).round() as u64;
    let mem = |m: u64| giverny_core::limits::Mem(m).to_string();
    Some(format!(
        "the last {} {what} peaked at {} (median), {} at most",
        peaks.len(),
        mem(med),
        mem(max)
    ))
}

/// The high end of what tasks like this one peak at, MiB: the 90th
/// percentile (nearest rank) of the peaks [`peak_hint`] summarises. What the
/// ledger counts a task at when it admits memory. `None`
/// with too little history.
pub fn expected_peak_mb(history: &[Record], repo: Option<&str>, kind: Option<&str>) -> Option<u64> {
    let (_, mut peaks) = peak_sample(history, repo, kind)?;
    peaks.sort_unstable();
    let rank = (peaks.len() * 9).div_ceil(10).max(1);
    peaks.get(rank - 1).copied()
}

/// The cores a task used over the time it held its lease, in thousandths:
/// its `run` commands' CPU time over its wall time (start to landing).
/// `None` for a task with no CPU time recorded.
pub fn cpu_use_m(r: &Record) -> Option<u64> {
    let cpu = r.cpu_s?;
    (r.wall_s > 0).then(|| (cpu * 1000).div_ceil(r.wall_s))
}

/// The high end of the cores tasks like this one use over their life, in
/// thousandths: the 90th percentile (nearest rank) of [`cpu_use_m`] over
/// the same levels and as many tasks as [`expected_peak_mb`]. What the
/// ledger counts a lease at when it admits CPU. `None` with
/// too little history.
pub fn expected_cpu_m(history: &[Record], repo: Option<&str>, kind: Option<&str>) -> Option<u64> {
    let (_, mut uses) = sample(history, repo, kind, &cpu_use_m)?;
    uses.sort_unstable();
    let rank = (uses.len() * 9).div_ceil(10).max(1);
    uses.get(rank - 1).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(repo: &str, kind: Option<&str>, est_m: u64, work_m: u64) -> Record {
        Record {
            key: "t".into(),
            repo: Some(repo.into()),
            kind: kind.map(String::from),
            estimate_s: Some(est_m * 60),
            wall_s: work_m * 60,
            work_s: work_m * 60,
            outcome: Some("Done".into()),
            ..Record::default()
        }
    }

    #[test]
    fn the_narrowest_level_with_enough_history_wins() {
        let mut h = Vec::new();
        // demo FEATUREs run at 0.4×, demo BUGs at 1×, acme at 2×.
        for _ in 0..5 {
            h.push(rec("demo", Some("FEATURE"), 50, 20));
            h.push(rec("demo", Some("BUG"), 20, 20));
            h.push(rec("acme", Some("BUG"), 10, 20));
        }
        let told = |repo, kind| track_record(&h, Track::Guess, repo, kind).unwrap();
        assert_eq!(
            told(Some("demo"), Some("FEATURE")),
            "your gut estimates on FEATURE tasks in demo have usually run about 2.5× too high \
             (last 5, median)"
        );
        // No RESEARCH history in demo: repo alone, median of 0.4 and 1.0.
        assert_eq!(
            told(Some("demo"), Some("RESEARCH")),
            "your gut estimates in demo have usually run about 1.4× too high (last 10, median)"
        );
        // A repo with no history at all: everything (0.4, 1, 2 → 1).
        assert_eq!(
            told(Some("planets"), Some("BUG")),
            "your gut estimates have usually been about right (last 15, median)"
        );
        // No repo known: straight to everything.
        assert_eq!(told(None, Some("BUG")), told(Some("planets"), None));
    }

    #[test]
    fn append_and_load_round_trip_and_skip_bad_lines() {
        let d = std::env::temp_dir().join(format!("giverny-hist-io-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let f = d.join(FILE);
        let a = rec("g", Some("BUG"), 10, 5);
        append(&f, &a).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f)
            .unwrap()
            .write_all(b"not json\n\n")
            .unwrap();
        append(&f, &rec("g", None, 20, 5)).unwrap();
        let got = load(&f);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], a);
        assert!(load(&d.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
