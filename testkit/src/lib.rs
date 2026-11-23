//! Minimal property-testing helpers built on the standard library only.
//!
//! `Rng` is a SplitMix64 generator. `check` runs a property many times with a
//! seed taken from the `PBT_SEED` environment variable (or the clock), and on
//! failure panics with the property name, seed, iteration, and error message so
//! the run can be reproduced with `$env:PBT_SEED=<seed>; cargo test <name>`.

use std::panic::{self, AssertUnwindSafe};
use std::time::{SystemTime, UNIX_EPOCH};

/// SplitMix64 pseudo-random number generator. Deterministic for a given seed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `lo..=hi_inclusive`. Panics if `lo > hi_inclusive`.
    pub fn range(&mut self, lo: u64, hi_inclusive: u64) -> u64 {
        assert!(lo <= hi_inclusive, "Rng::range: lo ({lo}) > hi ({hi_inclusive})");
        let span = hi_inclusive - lo;
        if span == u64::MAX {
            return self.next_u64();
        }
        let n = span + 1;
        // Rejection sampling to avoid modulo bias.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return lo + v % n;
            }
        }
    }

    pub fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Random bytes with a length in `0..=max_len`.
    pub fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.range(0, max_len as u64) as usize;
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    /// Uniformly chosen element. Panics if `items` is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        assert!(!items.is_empty(), "Rng::pick: empty slice");
        let i = self.range(0, items.len() as u64 - 1) as usize;
        &items[i]
    }

    /// String of length `min..=max` drawn from `alphabet`.
    /// Panics if `alphabet` is empty, not ASCII, or `min > max`.
    pub fn ascii_from(&mut self, alphabet: &[u8], min: usize, max: usize) -> String {
        assert!(alphabet.is_ascii(), "Rng::ascii_from: alphabet must be ASCII");
        let len = self.range(min as u64, max as u64) as usize;
        (0..len).map(|_| *self.pick(alphabet) as char).collect()
    }
}

/// Seed from env `PBT_SEED` if set and valid, otherwise from the clock.
fn seed_from_env_or_clock() -> u64 {
    if let Ok(s) = std::env::var("PBT_SEED") {
        match s.trim().parse::<u64>() {
            Ok(seed) => return seed,
            Err(_) => eprintln!("testkit: ignoring invalid PBT_SEED={s:?}"),
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Runs `prop` `iterations` times with one `Rng` seeded from `PBT_SEED` or the clock.
///
/// Panics on the first `Err` (or panic inside `prop`) with the property name,
/// seed, iteration, and message.
///
/// ```
/// testkit::check("range stays in bounds", 100, |rng| {
///     let v = rng.range(3, 7);
///     if (3..=7).contains(&v) { Ok(()) } else { Err(format!("{v}")) }
/// });
/// ```
pub fn check<F: FnMut(&mut Rng) -> Result<(), String>>(name: &str, iterations: u32, mut prop: F) {
    let seed = seed_from_env_or_clock();
    let mut rng = Rng::new(seed);
    for i in 0..iterations {
        let msg = match panic::catch_unwind(AssertUnwindSafe(|| prop(&mut rng))) {
            Ok(Ok(())) => continue,
            Ok(Err(msg)) => msg,
            Err(payload) => format!("property panicked: {}", panic_text(&*payload)),
        };
        panic!(
            "property '{name}' failed (seed={seed}, iteration={i}/{iterations}): {msg}\n\
             reproduce with: $env:PBT_SEED={seed}; cargo test"
        );
    }
}
