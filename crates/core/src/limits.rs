//! What all managers on this machine together may use.
//!
//! [`Limits`] is the setting: CPU cores, RAM and GPUs, each `"auto"` unless
//! the person set a figure. It is serde-ready to sit in `config.toml` as
//! `[manager.limits]`; [`Limits::resolve`] turns every `auto` into a
//! number for the [`Machine`] it runs on (cores − 2, 70 % of RAM, 90 % of
//! each GPU's VRAM). [`Load`] is what the machine is doing right now, so a
//! grant can count programs that are not Giverny's.
//!
//! ```toml
//! [manager.limits]
//! cpu_cores = "auto"        # or 8
//! ram       = "auto"        # or "16G", "512M"; a bare number is GiB
//! gpus      = "auto"        # or [{ index = 0, vram = "20G" }]; [] = none
//! ```
//!
//! The ledger that hands these out is `giverny_claude::resources`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Cores held back from managers by `cpu_cores = "auto"`.
pub const AUTO_CORES_SPARE: u32 = 2;
/// The share of RAM `ram = "auto"` hands out, in percent.
pub const AUTO_RAM_PCT: u64 = 70;
/// The share of each GPU's VRAM `gpus = "auto"` hands out, in percent.
pub const AUTO_VRAM_PCT: u64 = 90;

/// `"auto"`, or a figure the person set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auto<T> {
    #[default]
    Auto,
    Set(T),
}

impl<T> Auto<T> {
    pub fn get(&self) -> Option<&T> {
        match self {
            Auto::Auto => None,
            Auto::Set(v) => Some(v),
        }
    }
}

impl<T: Serialize> Serialize for Auto<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Auto::Auto => s.serialize_str("auto"),
            Auto::Set(v) => v.serialize(s),
        }
    }
}

/// Only the word `auto` (any case).
struct AutoWord;

impl<'de> Deserialize<'de> for AutoWord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.trim().eq_ignore_ascii_case("auto") {
            Ok(AutoWord)
        } else {
            Err(serde::de::Error::custom("not `auto`"))
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Auto<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw<T> {
            Word(AutoWord),
            Val(T),
        }
        match Raw::<T>::deserialize(d) {
            Ok(Raw::Word(_)) => Ok(Auto::Auto),
            Ok(Raw::Val(v)) => Ok(Auto::Set(v)),
            Err(_) => Err(serde::de::Error::custom(
                "expected \"auto\" or a value (see [manager.limits])",
            )),
        }
    }
}

/// An amount of memory, in MiB. Written `"16G"`, `"1.5G"`, `"512M"`,
/// `"1T"`; a bare number is GiB (`3` = `"3G"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Mem(pub u64);

impl Mem {
    pub const fn mb(mb: u64) -> Mem {
        Mem(mb)
    }
    pub const fn gb(gb: u64) -> Mem {
        Mem(gb * 1024)
    }
    pub fn mb_value(self) -> u64 {
        self.0
    }

    /// `3G`, `3GB`, `3GiB`, `1.5g`, `512M`, `2T`, `3` (GiB). `None` for
    /// anything else.
    pub fn parse(s: &str) -> Option<Mem> {
        let s = s.trim().to_ascii_lowercase();
        let s = s
            .strip_suffix("ib")
            .or_else(|| s.strip_suffix('b'))
            .unwrap_or(&s);
        let (num, mul) = match s.chars().last()? {
            'k' => (&s[..s.len() - 1], 1.0 / 1024.0),
            'm' => (&s[..s.len() - 1], 1.0),
            'g' => (&s[..s.len() - 1], 1024.0),
            't' => (&s[..s.len() - 1], 1024.0 * 1024.0),
            _ => (s, 1024.0),
        };
        let n: f64 = num.trim().parse().ok()?;
        (n.is_finite() && n >= 0.0).then(|| Mem((n * mul).round() as u64))
    }
}

impl fmt::Display for Mem {
    /// `16G` when whole GiB, `1.5G` to a tenth, else `700M`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mb = self.0;
        if mb >= 1024 && mb.is_multiple_of(1024) {
            write!(f, "{}G", mb / 1024)
        } else if mb >= 1024 {
            write!(f, "{:.1}G", mb as f64 / 1024.0)
        } else {
            write!(f, "{mb}M")
        }
    }
}

impl Serialize for Mem {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.is_multiple_of(1024) {
            s.serialize_str(&format!("{}G", self.0 / 1024))
        } else {
            s.serialize_str(&format!("{}M", self.0))
        }
    }
}

impl<'de> Deserialize<'de> for Mem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Float(f64),
            Str(String),
        }
        let bad = || serde::de::Error::custom("a size like \"16G\" or \"512M\"");
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Mem::gb(n)),
            Raw::Float(f) => Mem::parse(&f.to_string()).ok_or_else(bad),
            Raw::Str(s) => Mem::parse(&s).ok_or_else(bad),
        }
    }
}

/// One GPU managers may use: its index (as `nvidia-smi` numbers it)
/// and how much of its VRAM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuLimit {
    pub index: u32,
    pub vram: Mem,
}

/// `[manager.limits]`: the whole machine's budget for managers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub cpu_cores: Auto<u32>,
    pub ram: Auto<Mem>,
    pub gpus: Auto<Vec<GpuLimit>>,
}

/// [`Limits`] with every `auto` turned into a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub cpu_cores: u32,
    pub ram: Mem,
    pub gpus: Vec<GpuLimit>,
}

impl Limits {
    /// Every `auto` as a number for `m`: cores − 2 (at least 1), 70 % of
    /// RAM, 90 % of each GPU's VRAM. A set figure is taken as given.
    pub fn resolve(&self, m: &Machine) -> Resolved {
        Resolved {
            cpu_cores: self
                .cpu_cores
                .get()
                .copied()
                .unwrap_or_else(|| m.cores.saturating_sub(AUTO_CORES_SPARE).max(1)),
            ram: self
                .ram
                .get()
                .copied()
                .unwrap_or(Mem(m.ram.0 * AUTO_RAM_PCT / 100)),
            gpus: self.gpus.get().cloned().unwrap_or_else(|| {
                m.gpus
                    .iter()
                    .map(|g| GpuLimit {
                        index: g.index,
                        vram: Mem(g.vram.0 * AUTO_VRAM_PCT / 100),
                    })
                    .collect()
            }),
        }
    }

    /// `[manager.limits]` from the TOML text of a `config.toml`; all
    /// `auto` when the table is absent. An unreadable table is an error, so
    /// a typo is said rather than silently ignored.
    ///
    /// Read through [`crate::config::ManagerConfig`], the same type
    /// `Config` mounts, so the ledger and the settings screen cannot read
    /// the table two ways. Only the `[manager]` table is looked at: a
    /// bad value elsewhere in the file is the settings screen's business,
    /// not a reason to refuse a claim.
    pub fn from_config_str(text: &str) -> Result<Limits, String> {
        let doc: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let Some(o) = doc.get("manager") else {
            return Ok(Limits::default());
        };
        o.clone()
            .try_into::<crate::config::ManagerConfig>()
            .map(|o| o.limits)
            .map_err(|e| format!("[manager.limits]: {e}"))
    }

    /// The limits in `config.toml` at `path`; all `auto` when the file or
    /// the table is absent.
    pub fn load_from(path: &Path) -> Result<Limits, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                Limits::from_config_str(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(_) => Ok(Limits::default()),
        }
    }

    /// The limits in Giverny's own `config.toml` (`<config>/giverny/`).
    pub fn load() -> Result<Limits, String> {
        Limits::load_from(&default_config_path())
    }
}

/// A size as `config.toml` holds it: `"16G"` when whole GiB, else `"1536M"`
/// (what [`Mem`]'s `Serialize` writes, without the quotes).
pub fn mem_text(m: Mem) -> String {
    if m.0.is_multiple_of(1024) {
        format!("{}G", m.0 / 1024)
    } else {
        format!("{}M", m.0)
    }
}

/// `gpus` as a TOML literal: `[]` or `[{ index = 0, vram = "20G" }, …]`.
pub fn gpus_text(gpus: &[GpuLimit]) -> String {
    let items: Vec<String> = gpus
        .iter()
        .map(|g| format!("{{ index = {}, vram = \"{}\" }}", g.index, mem_text(g.vram)))
        .collect();
    format!("[{}]", items.join(", "))
}

/// `50%` → `Some(50.0)`; anything without a trailing `%` → `None`.
fn percent(s: &str) -> Option<Result<f64, String>> {
    let n = s.strip_suffix('%')?.trim();
    Some(match n.parse::<f64>() {
        Ok(p) if p.is_finite() && p > 0.0 && p <= 100.0 => Ok(p),
        _ => Err(format!("{s} is not a share between 0 and 100 %")),
    })
}

fn is_auto(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("auto")
}

/// What a person typed for `cpu_cores`: `auto` (or nothing), a number of
/// cores, or a share of this machine's (`50%`). A share is turned into
/// cores here, so the file holds a figure the ledger reads as given.
pub fn parse_cores(input: &str, m: &Machine) -> Result<Auto<u32>, String> {
    let s = input.trim();
    if is_auto(s) {
        return Ok(Auto::Auto);
    }
    let n = match percent(s) {
        Some(p) => ((m.cores as f64 * p? / 100.0).round() as u32).max(1),
        None => s
            .parse::<u32>()
            .map_err(|_| format!("`{s}`: a number of cores, a share like 50%, or auto"))?,
    };
    if n == 0 {
        return Err("at least one core".into());
    }
    if n > m.cores {
        return Err(format!("this machine has {} cores", m.cores));
    }
    Ok(Auto::Set(n))
}

/// What a person typed for `ram`: `auto`, a size (`16G`, `512M`, a bare
/// number is GiB), or a share of this machine's RAM (`50%`).
pub fn parse_ram(input: &str, m: &Machine) -> Result<Auto<Mem>, String> {
    let s = input.trim();
    if is_auto(s) {
        return Ok(Auto::Auto);
    }
    Ok(Auto::Set(parse_share_of(s, m.ram, "RAM")?))
}

/// A size or a share (`50%`) of `whole`, more than nothing and no more
/// than `whole`. A share is rounded to a whole GiB when it is that large.
pub fn parse_share_of(s: &str, whole: Mem, what: &str) -> Result<Mem, String> {
    let mem = match percent(s) {
        Some(p) => {
            let mb = (whole.0 as f64 * p? / 100.0) as u64;
            // 50 % of 23.5G is 11.75G; the file says "12G", not "12032M".
            if mb >= 4096 {
                let g = mb as f64 / 1024.0;
                let near = Mem::gb(g.round() as u64);
                if near > whole {
                    Mem::gb(g.floor() as u64)
                } else {
                    near
                }
            } else {
                Mem(mb)
            }
        }
        None => Mem::parse(s)
            .ok_or_else(|| format!("`{s}`: a size like 16G or 512M, a share like 50%, or auto"))?,
    };
    if mem.0 == 0 {
        return Err(format!("more than no {what}"));
    }
    if mem > whole {
        return Err(format!("this machine has {whole} of {what}"));
    }
    Ok(mem)
}

/// `<config dir>/giverny/config.toml`, the file `config.rs` reads.
pub fn default_config_path() -> PathBuf {
    crate::config::config_path(crate::state::Paths::default_dirs().base())
}

/// One GPU as the machine reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub vram: Mem,
}

/// What this machine has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// Logical CPUs (not the cgroup's share: a capped worker asking sees
    /// the machine).
    pub cores: u32,
    pub ram: Mem,
    pub gpus: Vec<Gpu>,
}

impl Machine {
    /// Cores, total RAM (`/proc/meminfo` on Linux, the OS's own figure
    /// elsewhere, which on macOS is `sysctl hw.memsize`), and NVIDIA GPUs
    /// from `nvidia-smi` when it is installed — else none.
    pub fn detect() -> Machine {
        Machine {
            gpus: detect_gpus(),
            ..Machine::detect_cpu_ram()
        }
    }

    /// [`Machine::detect`] without the GPUs: no `nvidia-smi`, so cheap
    /// enough for a figure drawn on screen.
    pub fn detect_cpu_ram() -> Machine {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        let cores = (sys.cpus().len() as u32).max(
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
        );
        let ram = meminfo_mb("MemTotal").map(Mem).unwrap_or_else(|| {
            sys.refresh_memory();
            Mem(sys.total_memory() / (1024 * 1024))
        });
        Machine {
            cores,
            ram,
            gpus: Vec::new(),
        }
    }
}

/// `nvidia-smi --query-gpu=index,name,memory.total`; none when it is not
/// there or says nothing usable.
pub fn detect_gpus() -> Vec<Gpu> {
    std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_nvidia_smi(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// `0, NVIDIA GeForce RTX 4090, 24564` per line (MiB).
pub fn parse_nvidia_smi(out: &str) -> Vec<Gpu> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split(',').map(str::trim);
            let index = f.next()?.parse().ok()?;
            let name = f.next()?.to_string();
            let vram = f.next()?.parse().ok()?;
            Some(Gpu {
                index,
                name,
                vram: Mem(vram),
            })
        })
        .collect()
}

/// What the machine is doing now, Giverny's workers and everyone else.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Load {
    /// RAM the kernel could hand out now without swapping (`MemAvailable`).
    pub mem_available: Option<Mem>,
    /// The 1-minute load average.
    pub load1: Option<f64>,
}

impl Load {
    pub fn sample() -> Load {
        let mem_available = meminfo_mb("MemAvailable").map(Mem).or_else(|| {
            let mut sys = sysinfo::System::new();
            sys.refresh_memory();
            let b = sys.available_memory();
            (b > 0).then_some(Mem(b / (1024 * 1024)))
        });
        let load1 = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse().ok())
            .or_else(|| {
                let l = sysinfo::System::load_average().one;
                (l > 0.0).then_some(l)
            });
        Load {
            mem_available,
            load1,
        }
    }
}

/// One `/proc/meminfo` field, in MiB. `None` off Linux.
fn meminfo_mb(field: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&text, field)
}

fn parse_meminfo(text: &str, field: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(field)?.strip_prefix(':')?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb / 1024)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_and_meminfo() {
        let g = parse_nvidia_smi("0, NVIDIA RTX 4090, 24564\n1, A100, 81920\nbad\n");
        assert_eq!(g.len(), 2);
        assert_eq!((g[1].index, g[1].vram), (1, Mem(81920)));
        assert!(parse_nvidia_smi("").is_empty());
        let mi = "MemTotal:       24117248 kB\nMemFree: 1 kB\nMemAvailable:   16777216 kB\n";
        assert_eq!(parse_meminfo(mi, "MemTotal"), Some(23552));
        assert_eq!(parse_meminfo(mi, "MemAvailable"), Some(16384));
        assert_eq!(parse_meminfo(mi, "Mem"), None);
    }
}
