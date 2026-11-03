//! 同时看当前 cgroup 和它的祖先，systemd 的子组也不能越过父组的内存限额。

use std::path::{Component, Path};

#[cfg(target_os = "linux")]
pub(super) fn memory_limit() -> Option<u64> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    membership
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ':');
            let hierarchy = fields.next()?;
            let controllers = fields.next()?;
            let path = fields.next()?;
            if hierarchy == "0" && controllers.is_empty() {
                lowest_limit(Path::new("/sys/fs/cgroup"), Path::new(path), "memory.max")
            } else if controllers.split(',').any(|c| c == "memory") {
                lowest_limit(
                    Path::new("/sys/fs/cgroup/memory"),
                    Path::new(path),
                    "memory.limit_in_bytes",
                )
            } else {
                None
            }
        })
        .min()
}

fn lowest_limit(root: &Path, membership: &Path, file: &str) -> Option<u64> {
    // cgroup namespace 可以把当前组挂成根目录。深路径不存在时，仍然检查挂载根的限额。
    if membership
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    let relative = membership.strip_prefix("/").unwrap_or(membership);
    let current = root.join(relative);
    current
        .ancestors()
        .take_while(|p| p.starts_with(root))
        .filter_map(|p| {
            std::fs::read_to_string(p.join(file))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|&n| n > 0)
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_groups_obey_the_smallest_ancestor_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("service/child")).unwrap();
        std::fs::write(root.join("memory.max"), "max\n").unwrap();
        std::fs::write(root.join("service/memory.max"), "536870912\n").unwrap();
        std::fs::write(root.join("service/child/memory.max"), "1073741824\n").unwrap();
        assert_eq!(
            lowest_limit(root, Path::new("/service/child"), "memory.max"),
            Some(512 << 20)
        );
    }

    #[test]
    fn a_namespaced_mount_root_and_v1_limits_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.limit_in_bytes"), "268435456").unwrap();
        assert_eq!(
            lowest_limit(
                dir.path(),
                Path::new("/unavailable/host/path"),
                "memory.limit_in_bytes"
            ),
            Some(256 << 20)
        );
        assert_eq!(
            lowest_limit(dir.path(), Path::new("/../escape"), "memory.limit_in_bytes"),
            None
        );
    }
}
