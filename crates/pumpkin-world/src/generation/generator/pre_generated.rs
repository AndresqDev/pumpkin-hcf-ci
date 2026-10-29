use pumpkin_data::dimension::Dimension;

use crate::generation::Seed;

/// Load-only generator used when world generation is disabled.
///
/// The server keeps loading chunks from disk. Any chunk that is not present is
/// handled by the chunk system as an empty void chunk, so this generator never
/// needs to run terrain, biome, feature, carver or structure generation.
#[derive(Clone, Debug)]
pub struct PreGeneratedGenerator {
    seed: Seed,
    dimension: Dimension,
}

impl PreGeneratedGenerator {
    #[must_use]
    pub const fn new(seed: Seed, dimension: Dimension) -> Self {
        Self { seed, dimension }
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed.0
    }

    #[must_use]
    pub const fn dimension(&self) -> &Dimension {
        &self.dimension
    }
}
