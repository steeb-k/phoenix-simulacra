//! Deterministic on-disk fixtures for verifying that a backup/restore/clone/
//! mount preserved file contents exactly.
//!
//! [`fill_fixture`] writes a reproducible tree (varied sizes, multi-chunk
//! files, and fragmentation induced by interleaved write/delete) into a drive
//! and returns a [`FixtureDigest`] mapping relative path -> BLAKE3. After a
//! round-trip, [`verify_fixture`] re-hashes the tree at a (possibly different)
//! drive letter and asserts every file matches.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Map of relative path (forward-slashed) -> BLAKE3 hex of the file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureDigest(pub BTreeMap<String, String>);

/// A tiny deterministic PRNG (SplitMix64) so fixtures are byte-identical
/// across runs without pulling in an rng crate.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&v[..n]);
        }
    }
}

/// Write the deterministic fixture tree under `<letter>:\phoenix-fixture` and
/// return its digest. `seed` makes the content reproducible.
pub fn fill_fixture(letter: char, seed: u64) -> Result<FixtureDigest> {
    let root = format!("{letter}:\\phoenix-fixture");
    let root = Path::new(&root);
    if root.exists() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::create_dir_all(root).context("creating fixture root")?;

    let mut rng = Rng::new(seed);

    // A spread of sizes: empty, sub-sector, multi-sector, and several that
    // exceed the 4 MiB container chunk so multi-chunk streams are exercised.
    let sizes: [usize; 8] = [
        0,
        1,
        4095,
        65_536,
        1_048_576,
        4 * 1024 * 1024 + 7, // spans a chunk boundary
        9 * 1024 * 1024 + 123,
        512,
    ];

    // Induce fragmentation: create filler files interleaved with the keepers,
    // then delete the fillers so the keepers land in non-contiguous clusters.
    for (i, &size) in sizes.iter().enumerate() {
        let filler = root.join(format!("filler_{i}.tmp"));
        let mut fbuf = vec![0u8; 256 * 1024];
        rng.fill(&mut fbuf);
        write_file(&filler, &fbuf)?;

        let keeper = root.join(format!("data_{i}.bin"));
        let mut buf = vec![0u8; size];
        rng.fill(&mut buf);
        write_file(&keeper, &buf)?;

        std::fs::remove_file(&filler).ok();
    }

    // A nested directory with a couple of files, to cover subdirectories.
    let sub = root.join("nested").join("deeper");
    std::fs::create_dir_all(&sub).context("creating nested dirs")?;
    for i in 0..3 {
        let mut buf = vec![0u8; 10_000 + i * 3000];
        rng.fill(&mut buf);
        write_file(&sub.join(format!("n_{i}.dat")), &buf)?;
    }

    hash_tree(root, root)
}

/// Write a fixture whose data is *severely* fragmented and spread over the
/// whole volume, for exercising NTFS shrink relocation.
///
/// [`fill_fixture`] fragments a little; this fragments enough to break things.
/// The recipe:
///
///   1. Fill most of the volume with alternating keeper/filler pairs, so the
///      two interleave across the entire address space.
///   2. Delete every filler, leaving holes from the bottom of the volume to
///      the top.
///   3. Write one large file into those holes. NTFS has nowhere contiguous to
///      put it, so it lands as hundreds of fragments reaching the very end of
///      the volume — a run list far too long for one MFT record, which NTFS
///      spills into `$ATTRIBUTE_LIST` extension records.
///   4. Delete the keepers, so the volume is now mostly empty but the one
///      surviving file still spans it end to end.
///
/// That last step is what makes the fixture useful: the used data easily fits
/// a much smaller partition, yet shrinking still has to relocate half of a
/// heavily fragmented file — which is exactly the case that used to fail with
/// "relocated run list grew past the attribute budget", and now has to
/// succeed.
///
/// `volume_mb` is the volume's size; the fill targets ~80% of it.
pub fn fill_fixture_fragmented(letter: char, seed: u64, volume_mb: u64) -> Result<FixtureDigest> {
    const UNIT: usize = 256 * 1024;

    let root = format!("{letter}:\\phoenix-fixture");
    let root = Path::new(&root);
    if root.exists() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::create_dir_all(root).context("creating fixture root")?;
    let mut rng = Rng::new(seed);

    let budget = (volume_mb * 1024 * 1024) * 80 / 100;
    let pairs = (budget / (2 * UNIT as u64)).max(8) as usize;

    // 1. Interleave keepers and fillers across the whole volume.
    let mut buf = vec![0u8; UNIT];
    for i in 0..pairs {
        rng.fill(&mut buf);
        write_file(&root.join(format!("k_{i:05}.tmp")), &buf)?;
        rng.fill(&mut buf);
        write_file(&root.join(format!("f_{i:05}.tmp")), &buf)?;
    }

    // 2. Punch out the fillers.
    for i in 0..pairs {
        std::fs::remove_file(root.join(format!("f_{i:05}.tmp")))
            .with_context(|| format!("removing filler {i}"))?;
    }

    // 3. One large file, which now has only those holes to live in. Written
    //    in UNIT-sized appends so it grows into the holes one at a time
    //    rather than letting NTFS reserve a contiguous range up front.
    let big = root.join("fragmented.bin");
    {
        let mut f =
            std::fs::File::create(&big).with_context(|| format!("creating {}", big.display()))?;
        for _ in 0..pairs {
            rng.fill(&mut buf);
            f.write_all(&buf)
                .with_context(|| format!("writing {}", big.display()))?;
        }
        f.flush().ok();
    }

    // 4. Drop the keepers. The volume is now mostly free, but `fragmented.bin`
    //    still reaches from the bottom of it to the top.
    for i in 0..pairs {
        std::fs::remove_file(root.join(format!("k_{i:05}.tmp")))
            .with_context(|| format!("removing keeper {i}"))?;
    }

    // A couple of ordinary files so the digest covers more than one shape.
    let sub = root.join("nested");
    std::fs::create_dir_all(&sub).context("creating nested dir")?;
    for i in 0..3 {
        let mut small = vec![0u8; 10_000 + i * 3000];
        rng.fill(&mut small);
        write_file(&sub.join(format!("n_{i}.dat")), &small)?;
    }

    hash_tree(root, root)
}

/// Re-hash the fixture tree at `letter` and compare against `expected`.
pub fn verify_fixture(letter: char, expected: &FixtureDigest) -> Result<()> {
    let root = format!("{letter}:\\phoenix-fixture");
    let root = Path::new(&root);
    if !root.exists() {
        bail!("fixture root {} missing after round-trip", root.display());
    }
    let got = hash_tree(root, root)?;
    if got != *expected {
        // Report the first divergence for a readable failure.
        for (path, want) in &expected.0 {
            match got.0.get(path) {
                None => bail!("file missing after round-trip: {path}"),
                Some(have) if have != want => {
                    bail!("content mismatch for {path}: expected {want}, got {have}")
                }
                _ => {}
            }
        }
        for path in got.0.keys() {
            if !expected.0.contains_key(path) {
                bail!("unexpected extra file after round-trip: {path}");
            }
        }
        bail!("fixture digests differ");
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.flush().ok();
    Ok(())
}

fn hash_tree(root: &Path, dir: &Path) -> Result<FixtureDigest> {
    let mut map = BTreeMap::new();
    hash_tree_into(root, dir, &mut map)?;
    Ok(FixtureDigest(map))
}

fn hash_tree_into(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            hash_tree_into(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            out.insert(rel, blake3::hash(&bytes).to_hex().to_string());
        }
    }
    Ok(())
}
