use serde::{Deserialize, Serialize};

/// Default HCF profile applied when `[hcf].profile` is not overridden.
const DEFAULT_HCF_PROFILE: &str = "hcf-1.8";

/// HCF runtime profile configuration.
///
/// The profile is disabled by default, so an unmodified `pumpkin.toml` keeps
/// the vanilla code path. Optional keys are resolved by the profile defaults;
/// an explicit value always wins.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HcfConfig {
    /// Whether the HCF profile is active.
    pub enabled: bool,
    /// Emit the per-section `HCF_METRIC` diagnostics.
    ///
    /// Instrumentation is deliberately opt-in: `Instant::now()` and the
    /// per-packet atomic updates are useful for A/B runs but are pure overhead
    /// for a production server. It has no effect when `enabled` is `false`.
    pub metrics: Option<bool>,
    /// Name of the profile that supplies the defaults.
    pub profile: String,

    /// Whether only the overworld dimension is processed.
    pub only_overworld: Option<bool>,

    /// Disables mob AI when `true`.
    pub disable_mob_ai: Option<bool>,
    /// Disables natural mob spawning when `true`.
    pub disable_natural_spawning: Option<bool>,
    /// Disables raids when `true`.
    pub disable_raids: Option<bool>,
    /// Disables village POI logic when `true`.
    pub disable_villages: Option<bool>,
    /// Disables weather when `true`.
    pub disable_weather: Option<bool>,
    /// Disables random block and fluid ticks when `true`.
    pub disable_random_ticks: Option<bool>,
    /// Disables advancement progress when `true`.
    pub disable_advancements: Option<bool>,
    /// Disables vanilla statistics when `true`.
    pub disable_statistics: Option<bool>,
    /// Disables the recipe book when `true`.
    pub disable_recipe_book: Option<bool>,
    /// Disables chat signing when `true`.
    pub disable_chat_signing: Option<bool>,
    /// Disables warden and sculk behaviour when `true`.
    pub disable_warden: Option<bool>,
    /// Disables map updates when `true`.
    pub disable_maps: Option<bool>,
    /// Disables runtime world generation when `true`.
    ///
    /// The server only loads chunks that already exist on disk. Missing chunks
    /// are served as empty void chunks and are not written back. A pre-generated
    /// world with a valid `level.dat` is required.
    pub disable_worldgen: Option<bool>,

    /// Enables the HCF chunk tick fast path when `true`.
    pub chunk_tick_fast_path: Option<bool>,
    /// Enables the HCF player tick fast path when `true`.
    pub player_tick_fast_path: Option<bool>,
    /// Enables the HCF entity tracker fast path when `true`.
    pub entity_tracker_fast_path: Option<bool>,
    /// Enables packet batching when `true`.
    pub packet_batch: Option<bool>,
    /// Scheduler budget in milliseconds, clamped to `0..=50`.
    pub scheduler_budget_ms: Option<u32>,
    /// Enables the scheduler running guard when `true` (default in `hcf-1.8`).
    pub scheduler_guard: Option<bool>,
    /// Inhabited-time update frequency in Hz, clamped to `1..=20`.
    pub inhabited_time_hz: Option<u32>,

    /// Whether player-player collisions are enabled (positive vanilla flag).
    pub player_player_collisions: Option<bool>,
    /// Whether player-mob collisions are enabled (positive vanilla flag).
    pub player_mob_collisions: Option<bool>,
}

impl Default for HcfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            metrics: None,
            profile: DEFAULT_HCF_PROFILE.to_string(),
            only_overworld: None,
            disable_mob_ai: None,
            disable_natural_spawning: None,
            disable_raids: None,
            disable_villages: None,
            disable_weather: None,
            disable_random_ticks: None,
            disable_advancements: None,
            disable_statistics: None,
            disable_recipe_book: None,
            disable_chat_signing: None,
            disable_warden: None,
            disable_maps: None,
            disable_worldgen: None,
            chunk_tick_fast_path: None,
            player_tick_fast_path: None,
            entity_tracker_fast_path: None,
            packet_batch: None,
            scheduler_budget_ms: None,
            scheduler_guard: None,
            inhabited_time_hz: None,
            player_player_collisions: None,
            player_mob_collisions: None,
        }
    }
}
