use std::num::NonZeroI32;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::BufMut;
use crossbeam::atomic::AtomicCell;
use dashmap::DashMap;
use dashmap::DashSet;
use pumpkin_protocol::bedrock::client::remove_actor::CRemoveActor;
use pumpkin_protocol::codec::item_stack_seralizer::ItemStackSerializer;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::codec::var_long::VarLong;
use pumpkin_protocol::java::client::play::{
    CEntityVelocity, CHeadRot, CRemoveEntities, CSetEntityMetadata, CSetEquipment, CSetPassengers,
    Metadata,
};
use pumpkin_protocol::{BClientPacket, ClientPacket};
use pumpkin_util::GameMode;
use pumpkin_util::math::get_section_cord;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::version::JavaMinecraftVersion;
use pumpkin_world::cylindrical_chunk_iterator::Cylindrical;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::entity::EntityBase;
use crate::entity::player::Player;
use crate::net::ClientPlatform;
use crate::net::java::JavaClient;
use crate::world::World;
use crate::world::chunker::get_view_distance;

/// HCF-CORE-022a: per-player state resolved once per tracker pass instead of once per pair.
///
/// `ready_chunks` is filled lazily: a player that never reaches the chunk-ready check (no
/// candidate entity in range) does not pay for a `chunk_sender` lock or a snapshot.
struct PlayerTrackerContext {
    player: Arc<Player>,
    view_distance: i32,
    watched_section: Cylindrical,
    chunk_epoch: u32,
    ready_chunks: Option<FxHashSet<Vector2<i32>>>,
}

impl PlayerTrackerContext {
    fn new(player: Arc<Player>) -> Self {
        // HCF-CORE-022a: same sources as `update_player`, sampled once for the whole pass.
        let view_distance = NonZeroI32::from(get_view_distance(&player)).get();
        let watched_section = player.watched_section.load();
        let chunk_epoch = player.chunk_send_epoch.load(Ordering::Relaxed);
        Self {
            player,
            view_distance,
            watched_section,
            chunk_epoch,
            ready_chunks: None,
        }
    }

    /// HCF-CORE-022a: at most one `chunk_sender` lock per player per tracker pass.
    ///
    /// The epoch is reloaded per check; while it does not change, `is_chunk_ready` is answered
    /// from the snapshot. A world change/teleport that bumps the epoch forces a fresh snapshot.
    fn is_chunk_ready(&mut self, pos: Vector2<i32>) -> bool {
        let epoch = self.player.chunk_send_epoch.load(Ordering::Relaxed);
        if self.ready_chunks.is_none() || epoch != self.chunk_epoch {
            let sender = self
                .player
                .chunk_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Re-read under the sender lock: `change_world_chunks` bumps the epoch before
            // resetting the sender, so this avoids keeping a pre-reset snapshot as current.
            let locked_epoch = self.player.chunk_send_epoch.load(Ordering::Relaxed);
            self.ready_chunks = Some(sender.ready_chunks_snapshot());
            self.chunk_epoch = locked_epoch;
        }
        self.ready_chunks
            .as_ref()
            .is_some_and(|chunks| chunks.contains(&pos))
    }
}

pub struct TrackedEntity {
    pub entity: Arc<dyn EntityBase>,
    pub entity_id: i32,
    pub tracking_range: u32,
    pub update_interval: u32,
    pub track_deltas: bool,
    pub seen_by: DashSet<Uuid>,
    pub last_section_pos: AtomicCell<Vector3<i32>>,
}

impl TrackedEntity {
    #[must_use]
    pub fn new(
        entity: Arc<dyn EntityBase>,
        range: u32,
        update_interval: u32,
        track_deltas: bool,
    ) -> Self {
        let entity_id = entity.get_entity().entity_id;
        let pos = entity.get_entity().pos.load();
        let last_section_pos = Vector3::new(
            get_section_cord(pos.x.floor() as i32),
            get_section_cord(pos.y.floor() as i32),
            get_section_cord(pos.z.floor() as i32),
        );
        Self {
            entity,
            entity_id,
            tracking_range: range,
            update_interval,
            track_deltas,
            seen_by: DashSet::new(),
            last_section_pos: AtomicCell::new(last_section_pos),
        }
    }

    fn collect_indirect_passengers(
        entity: &Arc<dyn EntityBase>,
        result: &mut Vec<Arc<dyn EntityBase>>,
    ) {
        if let Ok(passengers) = entity.get_entity().passengers.try_lock() {
            for passenger in passengers.iter() {
                result.push(passenger.clone());
                Self::collect_indirect_passengers(passenger, result);
            }
        }
    }

    #[must_use]
    pub fn get_effective_range(&self) -> u32 {
        // HCF-CORE-022a: no passenger means no Vec allocation and no Arc clones.
        // If the lock is contended we keep the vanilla behaviour: only the tracking range is used.
        let has_passengers = self
            .entity
            .get_entity()
            .passengers
            .try_lock()
            .is_ok_and(|passengers| !passengers.is_empty());
        if !has_passengers {
            return self.tracking_range;
        }

        let mut effective_range = self.tracking_range;
        let mut passengers = Vec::new();
        Self::collect_indirect_passengers(&self.entity, &mut passengers);
        for passenger in passengers {
            let passenger_range = passenger.get_entity().entity_type.client_tracking_range;
            if passenger_range > effective_range {
                effective_range = passenger_range;
            }
        }
        effective_range
    }

    fn broadcast_to_player(&self, player: &Player) -> bool {
        self.entity.get_player().is_none_or(|target_player| {
            player.gamemode.load() == GameMode::Spectator
                || target_player.gamemode.load() != GameMode::Spectator
        })
    }

    pub fn update_player(&self, player: &Arc<Player>, _world: &World) {
        if player.get_entity().entity_id == self.entity_id {
            return;
        }

        let player_entity = player.get_entity();
        let player_pos = player_entity.pos.load();
        let entity_pos = self.entity.get_entity().pos.load();
        let dx = player_pos.x - entity_pos.x;
        let dz = player_pos.z - entity_pos.z;
        let dist_sq = dx.mul_add(dx, dz * dz);

        let player_vd = get_view_distance(player).get() as i32;
        let effective_range = self.get_effective_range();
        let visible_range_blocks = f64::from((effective_range as i32).min(player_vd) * 16);
        let range_sq = visible_range_blocks * visible_range_blocks;

        let entity_chunk = self.entity.get_entity().chunk_pos.load();
        let in_view = player
            .watched_section
            .load()
            .is_within_distance(entity_chunk.x, entity_chunk.y);

        // Vanilla `isChunkTracked`: never spawn before the chunk packet.
        let is_visible = dist_sq <= range_sq
            && self.broadcast_to_player(player)
            && in_view
            && player
                .chunk_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_chunk_ready(&entity_chunk);

        if is_visible {
            if self.seen_by.insert(player.gameprofile.id) {
                self.add_pairing(player);
            }
        } else {
            self.remove_player(player);
        }
    }

    /// HCF-CORE-022a: same pairing contract as [`Self::update_player`], but reading the
    /// view distance, watched section and chunk readiness from the per-pass player context.
    ///
    /// The entity position, chunk and effective tracking range are loaded once for the whole
    /// pass: a player entity has one hundred candidate recipients per tick, so repeating those
    /// loads and the passenger lock for every pair is measurable overhead. This keeps the
    /// individual `update_player` contract (and its per-pair lock) untouched for
    /// `add_entity`/`pair_new_player_with_tracked_entities`.
    fn update_player_ctx_cached(
        &self,
        ctx: &mut PlayerTrackerContext,
        effective_range: u32,
        entity_pos: Vector3<f64>,
        entity_chunk: Vector2<i32>,
    ) {
        let player_entity = ctx.player.get_entity();
        if player_entity.entity_id == self.entity_id {
            return;
        }

        let player_pos = player_entity.pos.load();
        let dx = player_pos.x - entity_pos.x;
        let dz = player_pos.z - entity_pos.z;
        let dist_sq = dx.mul_add(dx, dz * dz);

        let visible_range_blocks = f64::from((effective_range as i32).min(ctx.view_distance) * 16);
        let range_sq = visible_range_blocks * visible_range_blocks;

        let in_view = ctx
            .watched_section
            .is_within_distance(entity_chunk.x, entity_chunk.y);

        // Vanilla `isChunkTracked`: never spawn before the chunk packet.
        let is_visible = dist_sq <= range_sq
            && self.broadcast_to_player(&ctx.player)
            && in_view
            && ctx.is_chunk_ready(entity_chunk);

        if is_visible {
            if self.seen_by.insert(ctx.player.gameprofile.id) {
                self.add_pairing(&ctx.player);
            }
        } else {
            self.remove_player(&ctx.player);
        }
    }

    pub fn update_players(&self, players: &[Arc<Player>], world: &World) {
        for player in players {
            self.update_player(player, world);
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn add_pairing(&self, player: &Arc<Player>) {
        player.client.try_enqueue_spawn_packet(&self.entity);
        player.try_restore_vehicle(&self.entity);

        if let Some(target_player) = self.entity.get_player() {
            let skin_parts = target_player.config.load().skin_parts;
            let target_entity = target_player.get_entity();
            let target_id = target_entity.entity_id;

            if let ClientPlatform::Java(client) = player.client.as_ref() {
                let version = client.version.load();
                if version >= JavaMinecraftVersion::V_1_21 {
                    let mut buf = Vec::new();
                    for meta in [
                        Metadata::new(
                            pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMISATION,
                            skin_parts,
                        ),
                        Metadata::new(
                            pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMIZATION_ID,
                            skin_parts,
                        ),
                    ] {
                        let _ = meta.write(&mut buf, &version);
                    }
                    buf.put_u8(255);
                    let meta_packet = CSetEntityMetadata::new(target_id.into(), buf.into());
                    if let Ok(packet_data) =
                        JavaClient::serialize_packet_for_version(&meta_packet, version)
                    {
                        client.try_enqueue_packet(packet_data);
                    }
                }

                let head_yaw = target_entity.head_yaw.load();
                let head_rot_packet = CHeadRot::new(
                    target_id.into(),
                    (head_yaw * 256.0 / 360.0).rem_euclid(256.0) as u8,
                );
                if let Ok(data) = client.serialize_packet(&head_rot_packet) {
                    client.try_enqueue_packet(data);
                }
            }
        } else if self.entity.get_living_entity().is_some()
            && let ClientPlatform::Java(client) = player.client.as_ref()
        {
            let head_yaw = self.entity.get_entity().head_yaw.load();
            let head_rot_packet = CHeadRot::new(
                self.entity_id.into(),
                (head_yaw * 256.0 / 360.0).rem_euclid(256.0) as u8,
            );
            if let Ok(data) = client.serialize_packet(&head_rot_packet) {
                client.try_enqueue_packet(data);
            }
        }

        let vel = self.entity.get_entity().velocity.load();
        if vel.length_squared() > 1e-4
            && let ClientPlatform::Java(client) = player.client.as_ref()
        {
            let motion = CEntityVelocity::new(self.entity_id.into(), vel);
            if let Ok(data) = client.serialize_packet(&motion) {
                client.try_enqueue_packet(data);
            }
        }

        if let ClientPlatform::Java(client) = player.client.as_ref() {
            let version = client.version.load();
            // TODO: Support older versions
            if version >= JavaMinecraftVersion::V_1_21
                && let Some(non_default) = self
                    .entity
                    .get_entity()
                    .synched_data
                    .get_non_default_values_for_version(&version)
            {
                let packet = CSetEntityMetadata::new(self.entity_id.into(), non_default);
                if let Ok(packet_data) = JavaClient::serialize_packet_for_version(&packet, version)
                {
                    client.try_enqueue_packet(packet_data);
                }
            }
        }

        if let Some(living) = self.entity.get_living_entity()
            && let Ok(equipment_guard) = living.entity_equipment.try_lock()
        {
            let mut equipment_list = Vec::new();
            for (slot, item_stack) in &equipment_guard.equipment {
                if !item_stack.is_empty() {
                    equipment_list.push((slot.discriminant(), item_stack.clone()));
                }
            }
            if !equipment_list.is_empty() {
                let equipment: Vec<(i8, ItemStackSerializer)> = equipment_list
                    .iter()
                    .map(|(slot, stack)| (*slot, ItemStackSerializer::from(stack.clone())))
                    .collect();
                let packet = CSetEquipment::new(self.entity_id.into(), equipment);
                if let ClientPlatform::Java(client) = player.client.as_ref()
                    && let Ok(data) = client.serialize_packet(&packet)
                {
                    client.try_enqueue_packet(data);
                }
            }
        }

        if let Ok(passengers) = self.entity.get_entity().passengers.try_lock()
            && !passengers.is_empty()
        {
            let passenger_ids: Vec<VarInt> = passengers
                .iter()
                .map(|p| VarInt(p.get_entity().entity_id))
                .collect();
            let packet = CSetPassengers::new(VarInt(self.entity_id), &passenger_ids);
            if let ClientPlatform::Java(client) = player.client.as_ref()
                && let Ok(data) = client.serialize_packet(&packet)
            {
                client.try_enqueue_packet(data);
            }
        }

        if let Ok(vehicle_guard) = self.entity.get_entity().vehicle.try_lock()
            && let Some(vehicle) = vehicle_guard.as_ref()
            && let Ok(vehicle_passengers) = vehicle.get_entity().passengers.try_lock()
        {
            let passenger_ids: Vec<VarInt> = vehicle_passengers
                .iter()
                .map(|p| VarInt(p.get_entity().entity_id))
                .collect();
            let packet =
                CSetPassengers::new(VarInt(vehicle.get_entity().entity_id), &passenger_ids);
            if let ClientPlatform::Java(client) = player.client.as_ref()
                && let Ok(data) = client.serialize_packet(&packet)
            {
                client.try_enqueue_packet(data);
            }
        }
    }

    pub fn remove_pairing(&self, player: &Player) {
        let entity_ids = [self.entity_id.into()];
        match player.client.as_ref() {
            ClientPlatform::Java(client) => {
                let packet = CRemoveEntities::new(&entity_ids);
                if let Ok(data) = client.serialize_packet(&packet) {
                    client.try_enqueue_packet(data);
                }
            }
            ClientPlatform::Bedrock(client) => {
                let packet = CRemoveActor::new(VarLong(i64::from(self.entity_id)));
                if let Ok(data) = client.serialize_packet(&packet) {
                    client.try_enqueue_packet(data);
                }
            }
        }
    }

    pub fn broadcast_removed(&self, world: &World) {
        let entity_ids = [self.entity_id.into()];
        let je_packet = CRemoveEntities::new(&entity_ids);
        let be_packet = CRemoveActor::new(VarLong(i64::from(self.entity_id)));

        let players = world.players.load();
        let recipients = players
            .iter()
            .filter(|p| self.seen_by.contains(&p.gameprofile.id));

        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();
        for p in recipients {
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }
        let recipients_by_version =
            World::collect_java_recipients_by_version(java_recipients.into_iter());
        World::broadcast_java_grouped(&je_packet, recipients_by_version);
        World::broadcast_bedrock_grouped(&be_packet, bedrock_recipients.into_iter());

        self.seen_by.clear();
    }

    /// Vanilla `TrackedEntity.removePlayer`: despawn on the client, only if it was paired.
    pub fn remove_player(&self, player: &Player) {
        if self.seen_by.remove(&player.gameprofile.id).is_some() {
            self.remove_pairing(player);
        }
    }

    pub fn send_to_tracking_players<P: ClientPacket + Sync>(&self, packet: &P, world: &World) {
        let players = world.players.load();
        let recipients = players
            .iter()
            .filter(|p| self.seen_by.contains(&p.gameprofile.id));
        let recipients_by_version = World::collect_java_recipients_by_version(recipients);
        World::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn send_to_tracking_players_bedrock<P: BClientPacket + Sync>(
        &self,
        packet: &P,
        world: &World,
    ) {
        let players = world.players.load();
        let recipients = players.iter().filter_map(|p| {
            if self.seen_by.contains(&p.gameprofile.id)
                && let ClientPlatform::Bedrock(client) = p.client.as_ref()
            {
                return Some(client);
            }
            None
        });
        World::broadcast_bedrock_grouped(packet, recipients);
    }

    pub fn send_to_tracking_players_editioned<J: ClientPacket + Sync, B: BClientPacket + Sync>(
        &self,
        je_packet: &J,
        be_packet: &B,
        world: &World,
    ) {
        let players = world.players.load();
        let recipients = players
            .iter()
            .filter(|p| self.seen_by.contains(&p.gameprofile.id));

        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();
        for p in recipients {
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }
        let recipients_by_version =
            World::collect_java_recipients_by_version(java_recipients.into_iter());
        World::broadcast_java_grouped(je_packet, recipients_by_version);
        World::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }

    pub fn send_to_tracking_players_and_self<P: ClientPacket + Sync>(
        &self,
        packet: &P,
        world: &World,
    ) {
        self.send_to_tracking_players(packet, world);
        if let Some(player) = self.entity.get_player() {
            player.try_send_client_packet(packet);
        }
    }

    pub fn send_to_tracking_players_and_self_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
    >(
        &self,
        je_packet: &J,
        be_packet: &B,
        world: &World,
    ) {
        self.send_to_tracking_players_editioned(je_packet, be_packet, world);
        if let Some(player) = self.entity.get_player() {
            player.try_enqueue_packet_editioned(je_packet, be_packet);
        }
    }

    pub fn send_to_tracking_players_filtered<P: ClientPacket + Sync, F: Fn(&Player) -> bool>(
        &self,
        packet: &P,
        world: &World,
        filter: F,
    ) {
        let players = world.players.load();
        let recipients = players
            .iter()
            .filter(|p| self.seen_by.contains(&p.gameprofile.id) && filter(p));
        let recipients_by_version = World::collect_java_recipients_by_version(recipients);
        World::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn send_to_tracking_players_filtered_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
        F: Fn(&Player) -> bool,
    >(
        &self,
        je_packet: &J,
        be_packet: &B,
        world: &World,
        filter: F,
    ) {
        let players = world.players.load();
        let recipients = players
            .iter()
            .filter(|p| self.seen_by.contains(&p.gameprofile.id) && filter(p));

        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();
        for p in recipients {
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }
        let recipients_by_version =
            World::collect_java_recipients_by_version(java_recipients.into_iter());
        World::broadcast_java_grouped(je_packet, recipients_by_version);
        World::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }
}

pub struct EntityTracker {
    pub entity_map: DashMap<i32, Arc<TrackedEntity>>,
}

impl Default for EntityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entity_map: DashMap::new(),
        }
    }

    #[must_use]
    pub fn get_tracked_entity(&self, entity_id: i32) -> Option<Arc<TrackedEntity>> {
        self.entity_map.get(&entity_id).map(|r| r.value().clone())
    }

    #[must_use]
    pub fn has_entity_with_id(&self, entity_id: i32) -> bool {
        self.entity_map.contains_key(&entity_id)
    }

    #[must_use]
    pub fn is_tracked_by_any_player(&self, entity_id: i32) -> bool {
        self.entity_map
            .get(&entity_id)
            .is_some_and(|t| !t.seen_by.is_empty())
    }

    pub fn for_each_entity_tracked_by<F: FnMut(&Arc<dyn EntityBase>)>(
        &self,
        player: &Player,
        mut f: F,
    ) {
        for entry in &self.entity_map {
            if entry.value().seen_by.contains(&player.gameprofile.id) {
                f(&entry.value().entity);
            }
        }
    }

    pub fn add_entity(&self, entity: &Arc<dyn EntityBase>, world: &World) {
        let entity_type = entity.get_entity().entity_type;
        let range = entity_type.client_tracking_range;
        if range == 0 {
            return;
        }
        let update_interval = entity_type.update_interval;
        let track_deltas = entity_type.track_deltas;
        let entity_id = entity.get_entity().entity_id;

        let tracked = Arc::new(TrackedEntity::new(
            entity.clone(),
            range,
            update_interval,
            track_deltas,
        ));
        self.entity_map.insert(entity_id, tracked.clone());

        let players = world.players.load();
        tracked.update_players(players.as_ref(), world);
    }

    /// Must only be called after the player's own `CLogin` packet has been sent.
    pub fn pair_new_player_with_tracked_entities(&self, player_arc: &Arc<Player>, world: &World) {
        let entity_id = player_arc.get_entity().entity_id;
        for entry in &self.entity_map {
            if *entry.key() != entity_id {
                entry.value().update_player(player_arc, world);
            }
        }
    }

    /// Pairs entities in chunks whose packet was just queued for `player`.
    pub fn update_player_chunks(
        &self,
        player: &Arc<Player>,
        _world: &World,
        chunks: &[Vector2<i32>],
    ) {
        let chunks: FxHashSet<_> = chunks.iter().copied().collect();
        let entity_id = player.get_entity().entity_id;
        // HCF-CORE-022a: one context (and at most one `chunk_sender` lock) for the whole batch.
        let mut ctx = PlayerTrackerContext::new(player.clone());
        for entry in &self.entity_map {
            let tracked = entry.value();
            if *entry.key() == entity_id {
                continue;
            }
            let entity = tracked.entity.get_entity();
            let entity_chunk = entity.chunk_pos.load();
            if !chunks.contains(&entity_chunk) {
                continue;
            }
            tracked.update_player_ctx_cached(
                &mut ctx,
                tracked.get_effective_range(),
                entity.pos.load(),
                entity_chunk,
            );
        }
    }

    pub fn remove_entity(&self, entity: &dyn EntityBase, world: &World) {
        let entity_id = entity.get_entity().entity_id;
        if let Some(player) = entity.get_player() {
            for entry in &self.entity_map {
                entry.value().remove_player(player);
            }
        }

        if let Some((_, tracked)) = self.entity_map.remove(&entity_id) {
            tracked.broadcast_removed(world);
        }
    }

    pub fn update_player_position(&self, player: &Arc<Player>, world: &World) {
        let pos = player.get_entity().pos.load();
        let new_pos = Vector3::new(
            get_section_cord(pos.x.floor() as i32),
            get_section_cord(pos.y.floor() as i32),
            get_section_cord(pos.z.floor() as i32),
        );
        let entity_id = player.get_entity().entity_id;
        if let Some(tracked) = self.entity_map.get(&entity_id) {
            tracked.last_section_pos.store(new_pos);
        }
        // HCF-CORE-022a: load the player list once and reuse per-player contexts across the loop.
        let players = world.players.load();
        let mut all_players_contexts: Option<Vec<PlayerTrackerContext>> = None;
        let mut moved_player_ctx = PlayerTrackerContext::new(player.clone());
        for entry in &self.entity_map {
            let tracked = entry.value();
            let entity = tracked.entity.get_entity();
            let entity_pos = entity.pos.load();
            let entity_chunk = entity.chunk_pos.load();
            let effective_range = tracked.get_effective_range();
            if *entry.key() == entity_id {
                let contexts = all_players_contexts.get_or_insert_with(|| {
                    players
                        .iter()
                        .map(|player| PlayerTrackerContext::new(player.clone()))
                        .collect()
                });
                for ctx in contexts {
                    tracked.update_player_ctx_cached(
                        ctx,
                        effective_range,
                        entity_pos,
                        entity_chunk,
                    );
                }
            } else {
                tracked.update_player_ctx_cached(
                    &mut moved_player_ctx,
                    effective_range,
                    entity_pos,
                    entity_chunk,
                );
            }
        }
    }

    pub fn update_entity_position(&self, entity: &dyn EntityBase, world: &World) {
        if let Some(tracked) = self.entity_map.get(&entity.get_entity().entity_id) {
            let pos = entity.get_entity().pos.load();
            let new_pos = Vector3::new(
                get_section_cord(pos.x.floor() as i32),
                get_section_cord(pos.y.floor() as i32),
                get_section_cord(pos.z.floor() as i32),
            );
            tracked.last_section_pos.store(new_pos);
            let players = world.players.load();
            tracked.update_players(players.as_ref(), world);
        }
    }

    pub fn update_all(&self, world: &World) {
        // HCF-CORE-022a: internal dispatch; the vanilla body below stays untouched.
        if world
            .server
            .upgrade()
            .is_some_and(|server| server.hcf_profile().fast_tracker())
        {
            self.update_all_hcf(world);
            return;
        }

        let players = world.players.load();
        let mut moved_players = Vec::new();

        for entry in &self.entity_map {
            let tracked = entry.value();
            let pos = tracked.entity.get_entity().pos.load();
            let new_pos = Vector3::new(
                get_section_cord(pos.x.floor() as i32),
                get_section_cord(pos.y.floor() as i32),
                get_section_cord(pos.z.floor() as i32),
            );
            let old_pos = tracked.last_section_pos.load();
            if old_pos != new_pos {
                tracked.update_players(players.as_ref(), world);
                if let Some(player) = tracked.entity.get_player()
                    && let Some(player_arc) = world.get_player_by_uuid(player.gameprofile.id)
                {
                    moved_players.push(player_arc);
                }
                tracked.last_section_pos.store(new_pos);
            }
        }

        if !moved_players.is_empty() {
            for entry in &self.entity_map {
                entry.value().update_players(&moved_players, world);
            }
        }

        for entry in &self.entity_map {
            let tracked = entry.value();
            if tracked.entity.get_entity().synched_data.is_dirty() {
                tracked.entity.get_entity().send_dirty_entity_data();
            }
        }
    }

    /// HCF-CORE-022a: tracker fast path.
    ///
    /// It reproduces the three phases of the vanilla `update_all` (moved entities × all players,
    /// all entities × moved players, dirty metadata) with the same pairing decisions. The only
    /// differences are caches: one context per player per tick, no `get_player_by_uuid` per moved
    /// entity (the snapshot map is used) and at most one `chunk_sender` lock per player per pass.
    fn update_all_hcf(&self, world: &World) {
        let players = world.players.load();
        let mut contexts: Vec<PlayerTrackerContext> = players
            .iter()
            .map(|player| PlayerTrackerContext::new(player.clone()))
            .collect();
        // Vanilla's first phase always uses the player snapshot loaded at the start of the tick.
        let snapshot_contexts = contexts.len();
        let mut context_by_uuid: FxHashMap<Uuid, usize> = players
            .iter()
            .enumerate()
            .map(|(index, player)| (player.gameprofile.id, index))
            .collect();
        let mut moved_contexts = Vec::new();

        for entry in &self.entity_map {
            let tracked = entry.value();
            let pos = tracked.entity.get_entity().pos.load();
            let new_pos = Vector3::new(
                get_section_cord(pos.x.floor() as i32),
                get_section_cord(pos.y.floor() as i32),
                get_section_cord(pos.z.floor() as i32),
            );
            let old_pos = tracked.last_section_pos.load();
            if old_pos != new_pos {
                let entity = tracked.entity.get_entity();
                let entity_pos = entity.pos.load();
                let entity_chunk = entity.chunk_pos.load();
                let effective_range = tracked.get_effective_range();
                for ctx in &mut contexts[..snapshot_contexts] {
                    tracked.update_player_ctx_cached(
                        ctx,
                        effective_range,
                        entity_pos,
                        entity_chunk,
                    );
                }
                if let Some(player) = tracked.entity.get_player() {
                    let uuid = player.gameprofile.id;
                    let index = context_by_uuid.get(&uuid).copied().or_else(|| {
                        // Rare race: the player joined after the tick snapshot. Keep
                        // vanilla's fresh lookup instead of dropping the second phase.
                        let player = world.get_player_by_uuid(uuid)?;
                        contexts.push(PlayerTrackerContext::new(player));
                        let index = contexts.len() - 1;
                        context_by_uuid.insert(uuid, index);
                        Some(index)
                    });
                    if let Some(index) = index {
                        moved_contexts.push(index);
                    }
                }
                tracked.last_section_pos.store(new_pos);
            }
        }

        if !moved_contexts.is_empty() {
            for entry in &self.entity_map {
                let tracked = entry.value();
                let entity = tracked.entity.get_entity();
                let entity_pos = entity.pos.load();
                let entity_chunk = entity.chunk_pos.load();
                let effective_range = tracked.get_effective_range();
                for &index in &moved_contexts {
                    tracked.update_player_ctx_cached(
                        &mut contexts[index],
                        effective_range,
                        entity_pos,
                        entity_chunk,
                    );
                }
            }
        }

        for entry in &self.entity_map {
            let tracked = entry.value();
            if tracked.entity.get_entity().synched_data.is_dirty() {
                tracked.entity.get_entity().send_dirty_entity_data();
            }
        }
    }
}
