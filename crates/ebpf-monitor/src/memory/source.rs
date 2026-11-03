use super::Sample;
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid memory observation")
}
fn number(value: &str) -> io::Result<u64> {
    value.trim().parse().map_err(|_| invalid())
}
fn value(text: &str, key: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(key)).then(|| fields.next().ok_or_else(invalid).and_then(number))
        })
        .ok_or_else(invalid)?
}
fn limit(path: &Path) -> io::Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim() == "max" => Ok(None),
        Ok(text) => number(&text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn psi(text: &str, key: &str) -> io::Result<f64> {
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some(key))
        .ok_or_else(invalid)?;
    let n: f64 = line
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if !n.is_finite() || !(0. ..=100.).contains(&n) {
        return Err(invalid());
    }
    Ok(n)
}
pub(super) struct Source {
    cgroup: Option<PathBuf>,
    cgroup_root: PathBuf,
    meminfo: PathBuf,
}
impl Source {
    pub(super) fn for_pid(pid: u32) -> io::Result<Self> {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        let path = text.lines().find_map(|l| l.strip_prefix("0::"));
        let cgroup = path
            .map(|p| {
                let p = Path::new(p);
                if !p.is_absolute()
                    || p.components()
                        .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
                {
                    return Err(invalid());
                }
                Ok(Path::new("/sys/fs/cgroup").join(p.strip_prefix("/").map_err(|_| invalid())?))
            })
            .transpose()?;
        Ok(Self {
            cgroup,
            cgroup_root: "/sys/fs/cgroup".into(),
            meminfo: "/proc/meminfo".into(),
        })
    }
    pub(super) fn read(&self) -> io::Result<Sample> {
        let mem = fs::read_to_string(&self.meminfo)?;
        let mut available = value(&mem, "MemAvailable:")?
            .checked_mul(1024)
            .ok_or_else(invalid)?;
        let mut oom = 0;
        let mut high = 0;
        let pressure = if let Some(group) = &self.cgroup {
            let events = fs::read_to_string(group.join("memory.events"))?;
            oom = value(&events, "oom")?.saturating_add(value(&events, "oom_kill")?);
            high = value(&events, "high")?;
            let mut ancestor = Some(group.as_path());
            while let Some(dir) = ancestor {
                if dir == self.cgroup_root {
                    break;
                }
                // memory.high throttles; it is not a hard allocation/OOM limit.
                let bound = limit(&dir.join("memory.max"))?;
                if let Some(bound) = bound {
                    let current = number(&fs::read_to_string(dir.join("memory.current"))?)?;
                    // Discount only inactive file cache; anonymous memory is not free headroom.
                    let inactive = value(
                        &fs::read_to_string(dir.join("memory.stat"))?,
                        "inactive_file",
                    )?;
                    available =
                        available.min(bound.saturating_sub(current.saturating_sub(inactive)));
                }
                ancestor = dir.parent();
            }
            fs::read_to_string(group.join("memory.pressure"))
        } else {
            fs::read_to_string("/proc/pressure/memory")
        };
        let (some, full) = match pressure {
            Ok(text) => (psi(&text, "some")?, psi(&text, "full")?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (0., 0.), // older kernels: availability still sampled
            Err(e) => return Err(e),
        };
        Ok(Sample {
            available_pages: available / 4096,
            some,
            full,
            oom,
            high,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ebpf-memory-observations-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn ancestor_limits_discount_inactive_cache_and_ignore_soft_high_as_hard_cap() {
        let tmp = Scratch::new();
        let root = tmp.0.join("groups");
        let parent = root.join("parent");
        let child = parent.join("node");
        fs::create_dir_all(&child).unwrap();
        let meminfo = tmp.0.join("meminfo");
        fs::write(&meminfo, "MemAvailable: 1048576 kB\n").unwrap();
        for dir in [&parent, &child] {
            fs::write(dir.join("memory.max"), "104857600").unwrap();
            fs::write(dir.join("memory.high"), "1").unwrap();
            fs::write(dir.join("memory.current"), "94371840").unwrap();
            fs::write(dir.join("memory.stat"), "inactive_file 52428800\n").unwrap();
        }
        fs::write(child.join("memory.events"), "oom 0\noom_kill 0\nhigh 10\n").unwrap();
        fs::write(
            child.join("memory.pressure"),
            "some avg10=2.00\nfull avg10=0.25\n",
        )
        .unwrap();
        let source = Source {
            cgroup: Some(child.clone()),
            cgroup_root: root,
            meminfo,
        };
        assert_eq!(
            source.read().unwrap().available_pages,
            60 * 1024 * 1024 / 4096
        );
        fs::write(parent.join("memory.max"), "52428800").unwrap();
        assert_eq!(
            source.read().unwrap().available_pages,
            10 * 1024 * 1024 / 4096
        );
        fs::write(child.join("memory.events"), "oom bad\n").unwrap();
        assert!(source.read().is_err());
    }
    #[test]
    fn strict_parsers_do_not_turn_missing_observations_into_zero() {
        assert_eq!(
            value("MemAvailable: 123 kB\n", "MemAvailable:").unwrap(),
            123
        );
        assert!(value("MemFree: 1 kB", "MemAvailable:").is_err());
        assert!(psi("some avg10=NaN", "some").is_err());
        assert!(psi("some avg10=101", "some").is_err());
        assert_eq!(
            psi("some avg10=1.25 avg60=0 total=1", "some").unwrap(),
            1.25
        );
    }
}
