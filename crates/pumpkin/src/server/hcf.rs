//! Immutable HCF runtime profile, feature flags and build identity.
//!
//! This module is shared by the vanilla and HCF code paths. It is built once
//! from [`pumpkin_config::hcf::HcfConfig`] during server startup and never
//! reads the configuration again. With `hcf.enabled = false` every HCF query
//! is a no-op and the vanilla path is preserved.
//!
//! The build identity lets external tooling validate a binary before replacing
//! it. It is also logged once at startup in a stable, machine-readable line.

use pumpkin_config::hcf::HcfConfig;

/// Stable build identifier: `<CARGO_PKG_VERSION>+<GIT_HASH>`.
///
/// `GIT_HASH` may be `unknown` when the binary was built outside a git
/// checkout. The value is still valid, but not reproducible.
pub const PUMPKIN_BUILD_ID: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH"));

/// Version of the HCF core contract exposed to the updater and plugins.
pub const PUMPKIN_HCF_ABI: u32 = 1;

/// Name of the only profile currently implemented by the core.
const HCF_PROFILE_1_8: &str = "hcf-1.8";

/// Gameplay systems controlled by the HCF profile.
///
/// The discriminants are the bit positions used by [`HcfFeatureSet`]. They are
/// stable for the lifetime of [`PUMPKIN_HCF_ABI`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HcfFeature {
    /// Weather cycle and lightning.
    Weather = 0,
    /// Mob AI.
    MobAi,
    /// Natural mob spawning.
    NaturalSpawning,
    /// Raids and patrols.
    Raids,
    /// Village POI and villager logic.
    Villages,
    /// Dimension filtering (overworld only when enabled).
    Dimensions,
    /// Random block and fluid ticks.
    RandomTicks,
    /// Inhabited-time updates at a reduced frequency.
    InhabitedTime,
    /// Advancement progress.
    Advancements,
    /// Vanilla statistics.
    Statistics,
    /// Recipe book updates.
    RecipeBook,
    /// Chat signing.
    ChatSigning,
    /// Warden and sculk behaviour.
    Warden,
    /// Map updates.
    Maps,
    /// HCF chunk tick fast path.
    ChunkTickFastPath,
    /// HCF player tick fast path.
    PlayerTickFastPath,
    /// HCF entity tracker fast path.
    EntityTrackerFastPath,
    /// Packet batching.
    PacketBatch,
    /// Scheduler time budget.
    SchedulerBudget,
    /// Player-player collisions.
    PlayerPlayerCollisions,
    /// Player-mob collisions.
    PlayerMobCollisions,
}

impl HcfFeature {
    /// Number of features known to this ABI.
    pub const COUNT: u8 = 21;

    /// Returns the one-hot bit mask for this feature.
    #[inline]
    #[must_use]
    pub const fn bit(self) -> u64 {
        1u64 << (self as u8)
    }
}

/// Set of [`HcfFeature`] bits.
///
/// This is a plain `u64` wrapper instead of a new dependency. It is cheap to
/// copy and immutable queries never allocate or lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HcfFeatureSet(u64);

impl HcfFeatureSet {
    /// Returns an empty set.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Builds a set from a raw bitset.
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Returns a set containing every feature known to this ABI.
    #[inline]
    #[must_use]
    pub const fn all() -> Self {
        Self((1u64 << HcfFeature::COUNT) - 1)
    }

    /// Returns the raw bitset.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Returns whether no feature is set.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Inserts a feature.
    #[inline]
    pub const fn insert(&mut self, feature: HcfFeature) {
        self.0 |= feature.bit();
    }

    /// Removes a feature.
    #[inline]
    pub const fn remove(&mut self, feature: HcfFeature) {
        self.0 &= !feature.bit();
    }

    /// Returns whether a feature is set.
    #[inline]
    #[must_use]
    pub const fn contains(self, feature: HcfFeature) -> bool {
        self.0 & feature.bit() != 0
    }
}

/// Immutable runtime view of the HCF configuration.
///
/// The profile is stored as `Arc<HcfProfile>` on the server. Hot paths only do
/// cheap field/bitset reads: no locks and no access to `advanced_config`.
#[derive(Clone, Debug)]
pub struct HcfProfile {
    enabled: bool,
    profile: String,
    enabled_features: HcfFeatureSet,
    disabled_features: HcfFeatureSet,
    inhabited_time_hz: u32,
    scheduler_budget_ms: u32,
    scheduler_guard: bool,
    player_player_collisions: bool,
    player_mob_collisions: bool,
}

impl HcfProfile {
    /// Builds the immutable profile from configuration.
    ///
    /// HCF-CORE-005: this is the only place where `HcfConfig` is translated
    /// into feature bits. Defaults come from the `hcf-1.8` profile and explicit
    /// keys are applied afterwards; numeric values are clamped to their
    /// documented ranges.
    #[must_use]
    #[expect(clippy::too_many_lines)]
    pub fn from_config(config: &HcfConfig) -> Self {
        if config.enabled && config.profile != HCF_PROFILE_1_8 {
            tracing::warn!(
                "Unknown HCF profile '{}'; falling back to {HCF_PROFILE_1_8} defaults",
                config.profile
            );
        }

        // Defaults for `hcf-1.8`: work HCF does not use is disabled...
        let mut disabled_features = HcfFeatureSet::empty();
        for feature in [
            HcfFeature::MobAi,
            HcfFeature::NaturalSpawning,
            HcfFeature::Raids,
            HcfFeature::Villages,
            HcfFeature::Weather,
            HcfFeature::RandomTicks,
            HcfFeature::Advancements,
            HcfFeature::Statistics,
            HcfFeature::RecipeBook,
            HcfFeature::ChatSigning,
            HcfFeature::Warden,
            HcfFeature::Maps,
            HcfFeature::PlayerPlayerCollisions,
            HcfFeature::PlayerMobCollisions,
        ] {
            disabled_features.insert(feature);
        }

        // ...and the fast paths plus the reduced-frequency work are enabled.
        let mut enabled_features = HcfFeatureSet::empty();
        for feature in [
            HcfFeature::Dimensions,
            HcfFeature::InhabitedTime,
            HcfFeature::ChunkTickFastPath,
            HcfFeature::PlayerTickFastPath,
            HcfFeature::EntityTrackerFastPath,
            HcfFeature::PacketBatch,
            HcfFeature::SchedulerBudget,
        ] {
            enabled_features.insert(feature);
        }

        let inhabited_time_hz = config.inhabited_time_hz.unwrap_or(1).clamp(1, 20);
        let scheduler_budget_ms = config.scheduler_budget_ms.unwrap_or(5).min(50);
        let scheduler_guard = config.scheduler_guard.unwrap_or(true);

        resolve_enabled(
            &mut enabled_features,
            config.only_overworld,
            HcfFeature::Dimensions,
            true,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_mob_ai,
            HcfFeature::MobAi,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_natural_spawning,
            HcfFeature::NaturalSpawning,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_raids,
            HcfFeature::Raids,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_villages,
            HcfFeature::Villages,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_weather,
            HcfFeature::Weather,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_random_ticks,
            HcfFeature::RandomTicks,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_advancements,
            HcfFeature::Advancements,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_statistics,
            HcfFeature::Statistics,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_recipe_book,
            HcfFeature::RecipeBook,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_chat_signing,
            HcfFeature::ChatSigning,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_warden,
            HcfFeature::Warden,
            false,
        );
        resolve_disabled(
            &mut disabled_features,
            config.disable_maps,
            HcfFeature::Maps,
            false,
        );

        resolve_enabled(
            &mut enabled_features,
            config.chunk_tick_fast_path,
            HcfFeature::ChunkTickFastPath,
            true,
        );
        resolve_enabled(
            &mut enabled_features,
            config.player_tick_fast_path,
            HcfFeature::PlayerTickFastPath,
            true,
        );
        resolve_enabled(
            &mut enabled_features,
            config.entity_tracker_fast_path,
            HcfFeature::EntityTrackerFastPath,
            true,
        );
        resolve_enabled(
            &mut enabled_features,
            config.packet_batch,
            HcfFeature::PacketBatch,
            true,
        );

        // `scheduler_budget_ms = 0` means no budget, so the fast path is off.
        resolve_enabled(
            &mut enabled_features,
            config.scheduler_budget_ms.map(|_| scheduler_budget_ms > 0),
            HcfFeature::SchedulerBudget,
            true,
        );
        // At 20 Hz the vanilla per-tick update is already used.
        resolve_enabled(
            &mut enabled_features,
            config.inhabited_time_hz.map(|_| inhabited_time_hz < 20),
            HcfFeature::InhabitedTime,
            true,
        );

        // Positive collision flags: `true` keeps vanilla collisions. The
        // default profile disables both.
        resolve_collision(
            &mut disabled_features,
            config.player_player_collisions,
            HcfFeature::PlayerPlayerCollisions,
        );
        resolve_collision(
            &mut disabled_features,
            config.player_mob_collisions,
            HcfFeature::PlayerMobCollisions,
        );

        Self {
            enabled: config.enabled,
            profile: config.profile.clone(),
            enabled_features,
            disabled_features,
            inhabited_time_hz,
            scheduler_budget_ms,
            scheduler_guard,
            player_player_collisions: config.player_player_collisions.unwrap_or(false),
            player_mob_collisions: config.player_mob_collisions.unwrap_or(false),
        }
    }

    /// Returns whether the HCF profile is active.
    #[inline]
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the configured profile name.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.profile
    }

    /// Returns whether HCF disables the given phase.
    ///
    /// Always `false` when HCF is disabled, so an early-return never changes
    /// the vanilla path.
    #[inline]
    #[must_use]
    pub const fn disabled(&self, feature: HcfFeature) -> bool {
        self.enabled && self.disabled_features.contains(feature)
    }

    /// Returns whether HCF uses the fast path for the given phase.
    ///
    /// Always `false` when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn fast_path(&self, feature: HcfFeature) -> bool {
        self.enabled && self.enabled_features.contains(feature)
    }

    /// Returns the features that are active for this profile.
    ///
    /// Disabled features are excluded even if an override marked them as
    /// enabled, and the result is empty when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn active_features(&self) -> HcfFeatureSet {
        if !self.enabled {
            return HcfFeatureSet::empty();
        }
        HcfFeatureSet::from_bits(self.enabled_features.bits() & !self.disabled_features.bits())
    }

    /// Returns the active feature bits for the plugin/updater handshake.
    ///
    /// `0` when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn capabilities(&self) -> u64 {
        self.active_features().bits()
    }

    /// Convenience accessor for the chunk tick fast path.
    #[inline]
    #[must_use]
    pub const fn fast_chunks(&self) -> bool {
        self.fast_path(HcfFeature::ChunkTickFastPath)
    }

    /// Convenience accessor for the player tick fast path.
    #[inline]
    #[must_use]
    pub const fn fast_players(&self) -> bool {
        self.fast_path(HcfFeature::PlayerTickFastPath)
    }

    /// Convenience accessor for the entity tracker fast path.
    #[inline]
    #[must_use]
    pub const fn fast_tracker(&self) -> bool {
        self.fast_path(HcfFeature::EntityTrackerFastPath)
    }

    /// Convenience accessor for packet batching.
    #[inline]
    #[must_use]
    pub const fn packet_batch(&self) -> bool {
        self.fast_path(HcfFeature::PacketBatch)
    }

    /// Returns whether only the overworld should be loaded.
    ///
    /// HCF-CORE-010: driven by [`HcfFeature::Dimensions`], which the `hcf-1.8`
    /// profile enables by default and `only_overworld = false` can turn off.
    /// Always `false` when HCF is disabled, so the vanilla dimension list is
    /// preserved.
    #[inline]
    #[must_use]
    pub const fn only_overworld(&self) -> bool {
        self.fast_path(HcfFeature::Dimensions)
    }

    /// Returns whether player-player collisions are enabled.
    ///
    /// Vanilla collisions are always enabled when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn player_player_collisions_enabled(&self) -> bool {
        !self.enabled || self.player_player_collisions
    }

    /// Returns whether player-mob collisions are enabled.
    ///
    /// Vanilla collisions are always enabled when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn player_mob_collisions_enabled(&self) -> bool {
        !self.enabled || self.player_mob_collisions
    }

    /// Returns the inhabited-time update frequency in Hz.
    ///
    /// Vanilla updates every tick (20 Hz) when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn inhabited_time_hz(&self) -> u32 {
        if self.enabled {
            self.inhabited_time_hz
        } else {
            20
        }
    }

    /// Returns the scheduler budget in milliseconds.
    ///
    /// `0` means no budget and is returned when HCF is disabled.
    #[inline]
    #[must_use]
    pub const fn scheduler_budget_ms(&self) -> u32 {
        if self.enabled {
            self.scheduler_budget_ms
        } else {
            0
        }
    }

    /// Returns whether the scheduler running guard is enabled.
    ///
    /// Disabled when HCF is off so vanilla plugins keep their previous
    /// overlapping semantics.
    #[inline]
    #[must_use]
    pub const fn scheduler_guard(&self) -> bool {
        self.enabled && self.scheduler_guard
    }
}

/// Applies a `disable_*` override on top of a profile default.
///
/// `default_disabled` is the value used when the key is absent.
const fn resolve_disabled(
    features: &mut HcfFeatureSet,
    value: Option<bool>,
    feature: HcfFeature,
    default_disabled: bool,
) {
    let disabled = match value {
        Some(value) => value,
        None => default_disabled,
    };
    if disabled {
        features.insert(feature);
    } else {
        features.remove(feature);
    }
}

/// Applies a fast-path override on top of a profile default.
const fn resolve_enabled(
    features: &mut HcfFeatureSet,
    value: Option<bool>,
    feature: HcfFeature,
    default_enabled: bool,
) {
    let enabled = match value {
        Some(value) => value,
        None => default_enabled,
    };
    if enabled {
        features.insert(feature);
    } else {
        features.remove(feature);
    }
}

/// Applies a positive collision flag. `true` keeps vanilla collisions.
const fn resolve_collision(
    disabled_features: &mut HcfFeatureSet,
    value: Option<bool>,
    feature: HcfFeature,
) {
    if matches!(value, Some(true)) {
        disabled_features.remove(feature);
    } else if matches!(value, Some(false)) {
        disabled_features.insert(feature);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_profile_keeps_vanilla_defaults() {
        let profile = HcfProfile::from_config(&HcfConfig::default());

        assert!(!profile.enabled());
        assert!(!profile.disabled(HcfFeature::Weather));
        assert!(!profile.disabled(HcfFeature::Raids));
        assert!(!profile.fast_chunks());
        assert!(!profile.fast_players());
        assert!(!profile.only_overworld());
        assert_eq!(profile.inhabited_time_hz(), 20);
        assert_eq!(profile.scheduler_budget_ms(), 0);
        assert!(!profile.scheduler_guard());
        assert!(profile.player_player_collisions_enabled());
        assert!(profile.player_mob_collisions_enabled());
        assert!(profile.active_features().is_empty());
        assert_eq!(profile.capabilities(), 0);
    }

    #[test]
    fn hcf_1_8_defaults_match_contract() {
        let config = HcfConfig {
            enabled: true,
            ..HcfConfig::default()
        };
        let profile = HcfProfile::from_config(&config);

        assert!(profile.enabled());
        assert_eq!(profile.name(), "hcf-1.8");
        assert!(profile.disabled(HcfFeature::MobAi));
        assert!(profile.disabled(HcfFeature::NaturalSpawning));
        assert!(profile.disabled(HcfFeature::Raids));
        assert!(profile.disabled(HcfFeature::Weather));
        assert!(profile.disabled(HcfFeature::RandomTicks));
        assert!(profile.disabled(HcfFeature::Statistics));
        assert!(profile.disabled(HcfFeature::Advancements));
        assert!(profile.fast_chunks());
        assert!(profile.fast_players());
        assert!(profile.fast_tracker());
        assert!(profile.packet_batch());
        assert!(profile.only_overworld());
        assert_eq!(profile.inhabited_time_hz(), 1);
        assert_eq!(profile.scheduler_budget_ms(), 5);
        assert!(profile.scheduler_guard());
        assert!(!profile.player_player_collisions_enabled());
        assert!(!profile.player_mob_collisions_enabled());
    }

    #[test]
    fn explicit_overrides_win_over_profile_defaults() {
        let config = HcfConfig {
            enabled: true,
            only_overworld: Some(false),
            disable_weather: Some(false),
            scheduler_budget_ms: Some(0),
            scheduler_guard: Some(false),
            inhabited_time_hz: Some(20),
            player_player_collisions: Some(true),
            ..HcfConfig::default()
        };
        let profile = HcfProfile::from_config(&config);

        assert!(!profile.only_overworld());
        assert!(!profile.disabled(HcfFeature::Weather));
        assert!(!profile.fast_path(HcfFeature::SchedulerBudget));
        assert_eq!(profile.scheduler_budget_ms(), 0);
        assert!(!profile.scheduler_guard());
        assert!(profile.disabled(HcfFeature::InhabitedTime));
        assert_eq!(profile.inhabited_time_hz(), 20);
        assert!(profile.player_player_collisions_enabled());
    }
}
