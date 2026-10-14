//! Duplicate finder: same size → same first 64 KB → same full content.

use std::collections::HashMap;
use std::hash::Hasher;
use std::io::Read;
use std::path::Path;

use rayon::prelude::*;
use serde::Serialize;

use crate::index::Hit;

const HEAD: usize = 64 * 1024;
/// Groups returned at most (largest wasted space first).
const MAX_GROUPS: usize = 300;

#[derive(Debug, Serialize)]
pub struct DupGroup {
    pub size: u64,
    pub wasted: u64,
    pub files: Vec<Hit>,
}

fn hash_file(path: &str, limit: Option<usize>) -> Option<u64> {
    let mut f = std::fs::File::open(Path::new(path)).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = limit.unwrap_or(usize::MAX);
    while left > 0 {
        let want = buf.len().min(left);
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
        left -= n;
    }
    Some(h.finish())
}

/// Splits `files` into groups that share the same hash; singletons are dropped.
fn split_by(files: Vec<Hit>, limit: Option<usize>) -> Vec<Vec<Hit>> {
    let hashed: Vec<(u64, Hit)> = files.into_par_iter().filter_map(|f| hash_file(&f.path, limit).map(|h| (h, f))).collect();
    let mut m: HashMap<u64, Vec<Hit>> = HashMap::new();
    for (h, f) in hashed {
        m.entry(h).or_default().push(f);
    }
    m.into_values().filter(|g| g.len() > 1).collect()
}

pub fn find(candidates: Vec<Hit>) -> Vec<DupGroup> {
    let mut by_size: HashMap<u64, Vec<Hit>> = HashMap::new();
    for c in candidates {
        by_size.entry(c.size).or_default().push(c);
    }
    let mut groups: Vec<DupGroup> = by_size
        .into_values()
        .filter(|g| g.len() > 1)
        .collect::<Vec<_>>()
        .into_par_iter()
        .flat_map_iter(|g| {
            let size = g[0].size;
            let heads = split_by(g, Some(HEAD));
            heads
                .into_iter()
                .flat_map(move |h| if size as usize > HEAD { split_by(h, None) } else { vec![h] })
                .map(move |mut files| {
                    files.sort_by(|a, b| a.path.cmp(&b.path));
                    DupGroup { size, wasted: size * (files.len() as u64 - 1), files }
                })
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted));
    groups.truncate(MAX_GROUPS);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(p: &Path) -> Hit {
        let size = std::fs::metadata(p).unwrap().len();
        Hit { path: p.to_string_lossy().into_owned(), name: String::new(), dir: String::new(), size, mtime: 0, is_dir: false, cloud: false, score: 0 }
    }

    #[test]
    fn finds_identical_only() {
        let d = tempfile::tempdir().unwrap();
        let big_a: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut big_b = big_a.clone();
        *big_b.last_mut().unwrap() ^= 1; // same size and head, different tail
        std::fs::write(d.path().join("a1.bin"), &big_a).unwrap();
        std::fs::write(d.path().join("a2.bin"), &big_a).unwrap();
        std::fs::write(d.path().join("b.bin"), &big_b).unwrap();
        std::fs::write(d.path().join("s1.txt"), "same").unwrap();
        std::fs::write(d.path().join("s2.txt"), "same").unwrap();
        std::fs::write(d.path().join("s3.txt"), "diff").unwrap();
        let files = ["a1.bin", "a2.bin", "b.bin", "s1.txt", "s2.txt", "s3.txt"].iter().map(|n| hit(&d.path().join(n))).collect();
        let g = find(files);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].files.len(), 2);
        assert!(g[0].files[0].path.ends_with("a1.bin") && g[0].files[1].path.ends_with("a2.bin"));
        assert_eq!(g[0].wasted, 200_000);
        assert!(g[1].files.iter().all(|f| !f.path.ends_with("s3.txt")));
    }
}
