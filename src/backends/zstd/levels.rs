//! zstd per-level compression parameters, transcribed from zstd 1.5.7
//! `lib/compress/clevels.h` (the `srcSize > 256 KB` bucket).

/// zstd match-finding strategy family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strategy {
    Fast,
    Dfast,
    Greedy,
    Lazy,
    Lazy2,
    Btlazy2,
    Btopt,
    Btultra,
    Btultra2,
}

/// zstd compression parameters for one level.
#[derive(Debug, Clone, Copy)]
pub struct LevelParams {
    pub window_log: u32,
    pub chain_log: u32,
    pub hash_log: u32,
    pub search_log: u32,
    pub min_match: u32,
    pub target_len: u32,
    pub strategy: Strategy,
}

/// Levels 0..=22 (index 0 is the base row used for negative levels).
#[rustfmt::skip]
pub const DEFAULT_C_PARAMS: [LevelParams; 23] = [
    /* W,  C,  H,  S,  L,  TL, strat */
    LevelParams { window_log: 19, chain_log: 12, hash_log: 13, search_log: 1, min_match: 6, target_len:   1, strategy: Strategy::Fast     }, // base
    LevelParams { window_log: 19, chain_log: 13, hash_log: 14, search_log: 1, min_match: 7, target_len:   0, strategy: Strategy::Fast     }, // 1
    LevelParams { window_log: 20, chain_log: 15, hash_log: 16, search_log: 1, min_match: 6, target_len:   0, strategy: Strategy::Fast     }, // 2
    LevelParams { window_log: 21, chain_log: 16, hash_log: 17, search_log: 1, min_match: 5, target_len:   0, strategy: Strategy::Dfast    }, // 3
    LevelParams { window_log: 21, chain_log: 18, hash_log: 18, search_log: 1, min_match: 5, target_len:   0, strategy: Strategy::Dfast    }, // 4
    LevelParams { window_log: 21, chain_log: 18, hash_log: 19, search_log: 3, min_match: 5, target_len:   2, strategy: Strategy::Greedy   }, // 5
    LevelParams { window_log: 21, chain_log: 18, hash_log: 19, search_log: 3, min_match: 5, target_len:   4, strategy: Strategy::Lazy     }, // 6
    LevelParams { window_log: 21, chain_log: 19, hash_log: 20, search_log: 4, min_match: 5, target_len:   8, strategy: Strategy::Lazy     }, // 7
    LevelParams { window_log: 21, chain_log: 19, hash_log: 20, search_log: 4, min_match: 5, target_len:  16, strategy: Strategy::Lazy2    }, // 8
    LevelParams { window_log: 22, chain_log: 20, hash_log: 21, search_log: 4, min_match: 5, target_len:  16, strategy: Strategy::Lazy2    }, // 9
    LevelParams { window_log: 22, chain_log: 21, hash_log: 22, search_log: 5, min_match: 5, target_len:  16, strategy: Strategy::Lazy2    }, // 10
    LevelParams { window_log: 22, chain_log: 21, hash_log: 22, search_log: 6, min_match: 5, target_len:  16, strategy: Strategy::Lazy2    }, // 11
    LevelParams { window_log: 22, chain_log: 22, hash_log: 23, search_log: 6, min_match: 5, target_len:  32, strategy: Strategy::Lazy2    }, // 12
    LevelParams { window_log: 22, chain_log: 22, hash_log: 22, search_log: 4, min_match: 5, target_len:  32, strategy: Strategy::Btlazy2  }, // 13
    LevelParams { window_log: 22, chain_log: 22, hash_log: 23, search_log: 5, min_match: 5, target_len:  32, strategy: Strategy::Btlazy2  }, // 14
    LevelParams { window_log: 22, chain_log: 23, hash_log: 23, search_log: 6, min_match: 5, target_len:  32, strategy: Strategy::Btlazy2  }, // 15
    LevelParams { window_log: 22, chain_log: 22, hash_log: 22, search_log: 5, min_match: 5, target_len:  48, strategy: Strategy::Btopt    }, // 16
    LevelParams { window_log: 23, chain_log: 23, hash_log: 22, search_log: 5, min_match: 4, target_len:  64, strategy: Strategy::Btopt    }, // 17
    LevelParams { window_log: 23, chain_log: 23, hash_log: 22, search_log: 6, min_match: 3, target_len:  64, strategy: Strategy::Btultra  }, // 18
    LevelParams { window_log: 23, chain_log: 24, hash_log: 22, search_log: 7, min_match: 3, target_len: 256, strategy: Strategy::Btultra2 }, // 19
    LevelParams { window_log: 25, chain_log: 25, hash_log: 23, search_log: 7, min_match: 3, target_len: 256, strategy: Strategy::Btultra2 }, // 20
    LevelParams { window_log: 26, chain_log: 26, hash_log: 24, search_log: 7, min_match: 3, target_len: 512, strategy: Strategy::Btultra2 }, // 21
    LevelParams { window_log: 27, chain_log: 27, hash_log: 25, search_log: 9, min_match: 3, target_len: 999, strategy: Strategy::Btultra2 }, // 22
];

pub const MIN_LEVEL: i32 = 1;
pub const MAX_LEVEL: i32 = 22;

pub fn params_for(level: i32) -> LevelParams {
    DEFAULT_C_PARAMS[level.clamp(MIN_LEVEL, MAX_LEVEL) as usize]
}

/// Coarser grouping of strategies with similar deep-history behavior. The
/// whole-file history effect (deep windows, block correlations) is shared
/// within a family but can differ in sign across families — e.g. dfast's
/// blind far matches hurt on some data while fast's sparse table helps —
/// so anchor calibration is fitted per family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Fast,
    Dfast,
    Lazy,
    Btlazy2,
    Bt,
}

pub fn family_of(level: i32) -> Family {
    match params_for(level).strategy {
        Strategy::Fast => Family::Fast,
        Strategy::Dfast => Family::Dfast,
        Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 => Family::Lazy,
        Strategy::Btlazy2 => Family::Btlazy2,
        _ => Family::Bt,
    }
}

/// Deepest chain-walk tier we record during the scan.
pub const MAX_DEPTH_TIER: usize = 6;
/// Recorded depth prefixes: 1, 2, 4, 8, 16, 32, 64.
pub const DEPTH_TIERS: [u32; MAX_DEPTH_TIER + 1] = [1, 2, 4, 8, 16, 32, 64];

impl Strategy {
    /// How many chain candidates this strategy effectively examines relative
    /// to `2^search_log`, calibrated so bt* families (binary-tree search,
    /// far more effective per candidate) count for more.
    fn search_multiplier(self) -> f64 {
        match self {
            Strategy::Fast => 0.5,
            Strategy::Dfast => 1.0,
            Strategy::Greedy => 1.0,
            Strategy::Lazy | Strategy::Lazy2 => 1.0,
            Strategy::Btlazy2 => 2.0,
            Strategy::Btopt => 4.0,
            Strategy::Btultra => 8.0,
            Strategy::Btultra2 => 8.0,
        }
    }

    /// Lazy parsing lookahead depth: 0 = greedy, 1 = lazy, 2 = lazy2/bt*.
    pub fn lazy_depth(self) -> u32 {
        match self {
            Strategy::Fast | Strategy::Dfast | Strategy::Greedy => 0,
            Strategy::Lazy => 1,
            Strategy::Lazy2 | Strategy::Btlazy2 => 2,
            Strategy::Btopt | Strategy::Btultra | Strategy::Btultra2 => 2,
        }
    }
}

impl LevelParams {
    /// Effective chain depth for the replay, mapped onto the deepest recorded
    /// tier that does not exceed it (never overestimate search effort).
    pub fn depth_tier(&self) -> usize {
        let want = (1u64 << self.search_log) as f64 * self.strategy.search_multiplier();
        let mut tier = 0;
        for (t, &d) in DEPTH_TIERS.iter().enumerate() {
            if d as f64 <= want {
                tier = t;
            }
        }
        tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_tiers_monotonic_in_family() {
        // Within the lazy2 family, higher level never means shallower search.
        let mut prev = 0;
        for level in 8..=12 {
            let t = params_for(level).depth_tier();
            assert!(t >= prev);
            prev = t;
        }
    }

    #[test]
    fn level_params_sane() {
        for l in MIN_LEVEL..=MAX_LEVEL {
            let p = params_for(l);
            assert!(p.window_log >= 10 && p.window_log <= 27);
        }
    }
}
