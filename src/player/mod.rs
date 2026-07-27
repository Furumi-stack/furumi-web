use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use cot::db::Database;
use cot::http::StatusCode;
use cot::http::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderName, RANGE,
};
use cot::json::Json;
use cot::request::extractors::{Path, UrlQuery};
use cot::response::IntoResponse;
use cot::router::method::{delete, get, post};
use cot::router::{Route, Router};
use cot::session::Session;
use cot::{App, Body, Template};
use sqlx::Row as _;

use crate::auth;
use crate::config::AppConfig;
use crate::i18n::Translations;
use crate::lastfm::{LastfmClient, LastfmCredentials, LastfmTrackPayload};
use crate::scheduler::SchedulerHandle;
use crate::torrents::{TorrentPreviewRequest, TorrentService, TorrentStartRequest};

mod dto;
mod helpers;
mod queries;
mod rows;

use dto::*;
use helpers::{cover_variant_url, load_release_uploaders, track_cover_variant_url};
use queries::*;
use rows::*;

// ---------------------------------------------------------------------------
// JSON error helper
// ---------------------------------------------------------------------------

fn json_error(status: StatusCode, message: &str) -> cot::response::Response {
    let body = serde_json::json!({ "error": message });
    cot::http::Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::fixed(body.to_string()))
        .expect("valid response")
}

#[derive(serde::Serialize)]
struct LocalUploadResponse {
    ok: bool,
    filename: String,
    size: u64,
}

const PLAYER_DEVICE_TTL_MS: i64 = 30_000;
const PLAYER_DEVICE_COMMAND_TTL_MS: i64 = 20_000;
const PLAYER_DEVICE_MAX_COMMANDS: usize = 32;
const PLAYER_JAM_IDLE_TTL_MS: i64 = 4 * 60 * 60 * 1000;
const PLAYER_JAM_MAX_INVITEES: usize = 25;
const PLAYER_RADIO_TRACK_LIMIT: usize = 40;
const PLAYER_RADIO_CANDIDATE_LIMIT: i64 = 220;
const PLAYER_RADIO_RELEASE_SEED_TRACKS: i64 = 4;
const FED_DEVICE_PREFIX: &str = "fed:";

#[derive(Debug, Clone)]
struct PlayerDevice {
    id: String,
    name: String,
    kind: String,
    last_seen_ms: i64,
}

#[derive(Debug, Clone)]
struct PendingPlayerDeviceCommand {
    id: String,
    command: String,
    payload: serde_json::Value,
    created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlayerJamMemberStatus {
    Invited,
    Joined,
}

#[derive(Debug, Clone)]
struct PlayerJamMember {
    name: String,
    status: PlayerJamMemberStatus,
    last_seen_ms: i64,
}

#[derive(Debug, Clone)]
struct PlayerJamSession {
    id: String,
    host_user_id: i64,
    host_name: String,
    host_last_seen_ms: i64,
    members: HashMap<i64, PlayerJamMember>,
}

#[derive(Debug, Default)]
struct PlayerDeviceHubState {
    devices_by_user: HashMap<i64, HashMap<String, PlayerDevice>>,
    active_device_by_user: HashMap<i64, String>,
    commands_by_device: HashMap<(i64, String), VecDeque<PendingPlayerDeviceCommand>>,
    playback_state_by_user: HashMap<i64, PlayerDevicePlaybackStateDto>,
    jams_by_id: HashMap<String, PlayerJamSession>,
}

#[derive(Debug, Default)]
pub(crate) struct PlayerDeviceHub {
    state: Mutex<PlayerDeviceHubState>,
}

impl PlayerDeviceHub {
    pub(crate) fn shared() -> Arc<Self> {
        static HUB: OnceLock<Arc<PlayerDeviceHub>> = OnceLock::new();
        Arc::clone(HUB.get_or_init(|| Arc::new(PlayerDeviceHub::default())))
    }

    pub(crate) fn enqueue_fed_command(
        &self,
        user_id: i64,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<(), &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);
        let devices = state
            .devices_by_user
            .get(&user_id)
            .ok_or("no browser playback device")?;
        let target = state
            .active_device_by_user
            .get(&user_id)
            .filter(|id| !is_fed_virtual_device_id(id))
            .filter(|id| devices.contains_key(*id))
            .cloned()
            .or_else(|| {
                devices
                    .values()
                    .filter(|device| !is_fed_virtual_device_id(&device.id))
                    .max_by_key(|device| device.last_seen_ms)
                    .map(|device| device.id.clone())
            })
            .ok_or("no browser playback device")?;
        state.active_device_by_user.insert(user_id, target.clone());
        self.enqueue_command_locked(&mut state, user_id, &target, command, payload, now);
        Ok(())
    }

    pub(crate) fn current_playback_state_json(&self, user_id: i64) -> Option<serde_json::Value> {
        let state = self.state.lock().expect("player device hub lock");
        if state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|id| is_fed_virtual_device_id(id))
        {
            return None;
        }
        state
            .playback_state_by_user
            .get(&user_id)
            .and_then(|playback| serde_json::to_value(playback).ok())
    }

    pub(crate) fn playback_state_json_for_commands(
        &self,
        user_id: i64,
    ) -> Option<serde_json::Value> {
        let now = current_millis();
        let state = self.state.lock().expect("player device hub lock");
        state
            .playback_state_by_user
            .get(&user_id)
            .cloned()
            .map(|playback| playback_state_at(playback, now))
            .and_then(|playback| serde_json::to_value(playback).ok())
    }

    pub(crate) fn active_device_id_for_commands(&self, user_id: i64) -> Option<String> {
        let state = self.state.lock().expect("player device hub lock");
        state.active_device_by_user.get(&user_id).cloned()
    }

    pub(crate) fn fed_device_name_for_commands(
        &self,
        user_id: i64,
        fed_device_id: &str,
    ) -> Option<String> {
        let virtual_id = fed_virtual_device_id(fed_device_id);
        let state = self.state.lock().expect("player device hub lock");
        state
            .devices_by_user
            .get(&user_id)
            .and_then(|devices| devices.get(&virtual_id))
            .map(|device| device.name.clone())
    }

    pub(crate) fn apply_fed_playback_state_json(
        &self,
        user_id: i64,
        fed_device_id: &str,
        fed_device_name: &str,
        active: bool,
        payload: serde_json::Value,
    ) -> Result<(), &'static str> {
        let mut playback_state: PlayerDevicePlaybackStateDto =
            serde_json::from_value(payload).map_err(|_| "invalid playback state")?;
        let now = current_millis();
        playback_state.updated_at_ms = now;
        let virtual_id = fed_virtual_device_id(fed_device_id);
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);
        state.devices_by_user.entry(user_id).or_default().insert(
            virtual_id.clone(),
            PlayerDevice {
                id: virtual_id.clone(),
                name: fed_device_name.to_string(),
                kind: "fed".to_string(),
                last_seen_ms: now,
            },
        );
        // Match the trusted-device playback contract used by the TUI: a
        // background/stale active snapshot must not steal playback from a
        // browser that is actively playing. An explicit web handoff changes
        // `active_device_by_user` to the federated virtual device before the
        // snapshot arrives, so it still passes through here.
        let local_playback_is_protected = state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|active_id| !is_fed_virtual_device_id(active_id))
            && state
                .playback_state_by_user
                .get(&user_id)
                .is_some_and(|playback| playback.track.is_some() && !playback.paused);
        if active && local_playback_is_protected {
            return Ok(());
        }
        let should_update_playback = active
            || state
                .active_device_by_user
                .get(&user_id)
                .is_some_and(|active_id| active_id == &virtual_id);
        if active {
            state
                .active_device_by_user
                .insert(user_id, virtual_id.clone());
        }
        if should_update_playback {
            state.playback_state_by_user.insert(user_id, playback_state);
        }
        Ok(())
    }

    fn heartbeat(
        &self,
        user_id: i64,
        device_id: &str,
        user_agent: Option<&str>,
        current_jam_id: Option<&str>,
        playback_state: Option<PlayerDevicePlaybackStateDto>,
    ) -> PlayerDevicesResponse {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);
        self.touch_locked(&mut state, user_id, device_id, user_agent, now);
        self.update_playback_state_locked(&mut state, user_id, device_id, playback_state, now);
        self.touch_jam_locked(&mut state, user_id, device_id, current_jam_id, now);
        self.snapshot_locked(&state, user_id, device_id, current_jam_id, now)
    }

    fn poll(
        &self,
        user_id: i64,
        device_id: &str,
        user_agent: Option<&str>,
        current_jam_id: Option<&str>,
        playback_state: Option<PlayerDevicePlaybackStateDto>,
    ) -> PlayerDevicePollResponse {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);
        self.touch_locked(&mut state, user_id, device_id, user_agent, now);
        self.update_playback_state_locked(&mut state, user_id, device_id, playback_state, now);
        self.touch_jam_locked(&mut state, user_id, device_id, current_jam_id, now);
        let commands = state
            .commands_by_device
            .remove(&(user_id, device_id.to_string()))
            .unwrap_or_default()
            .into_iter()
            .map(|cmd| PlayerDeviceCommandDto {
                id: cmd.id,
                command: cmd.command,
                payload: cmd.payload,
            })
            .collect();
        let snapshot = self.snapshot_locked(&state, user_id, device_id, current_jam_id, now);
        PlayerDevicePollResponse {
            device_id: snapshot.device_id,
            active_device_id: snapshot.active_device_id,
            devices: snapshot.devices,
            jams: snapshot.jams,
            current_jam_id: snapshot.current_jam_id,
            commands,
            playback_state: snapshot.playback_state,
        }
    }

    fn select(
        &self,
        user_id: i64,
        current_device_id: &str,
        target_device_id: &str,
    ) -> Option<PlayerDevicesResponse> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);
        let devices = state.devices_by_user.get(&user_id)?;
        if !devices.contains_key(target_device_id) {
            return None;
        }
        let previous_active_id = state.active_device_by_user.get(&user_id).cloned();
        let transfer_state = state
            .playback_state_by_user
            .get(&user_id)
            .cloned()
            .map(|playback_state| playback_state_at(playback_state, now));
        state
            .active_device_by_user
            .insert(user_id, target_device_id.to_string());
        if previous_active_id.as_deref() != Some(target_device_id) {
            if let Some(transfer_state) = transfer_state {
                state
                    .playback_state_by_user
                    .insert(user_id, transfer_state.clone());
                if !is_fed_virtual_device_id(target_device_id)
                    && let Ok(payload) = serde_json::to_value(transfer_state)
                {
                    self.enqueue_command_locked(
                        &mut state,
                        user_id,
                        target_device_id,
                        "transfer_state",
                        payload,
                        now,
                    );
                }
            }
        }
        Some(self.snapshot_locked(&state, user_id, current_device_id, None, now))
    }

    fn enqueue_command(
        &self,
        user_id: i64,
        target_device_id: Option<&str>,
        jam_id: Option<&str>,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<(), &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);

        let (target_user_id, target_id) = if let Some(jam_id) = jam_id {
            let jam = state.jams_by_id.get(jam_id).ok_or("jam is not available")?;
            let member = jam.members.get(&user_id).ok_or("jam is not available")?;
            if member.status != PlayerJamMemberStatus::Joined {
                return Err("join the jam first");
            }
            let target_id = self
                .jam_target_device_id_locked(&state, jam)
                .ok_or("jam playback device is offline")?;
            (jam.host_user_id, target_id)
        } else {
            let target_id = match target_device_id {
                Some(id) => id.to_string(),
                None => state
                    .active_device_by_user
                    .get(&user_id)
                    .cloned()
                    .ok_or("no active device")?,
            };

            let devices = state
                .devices_by_user
                .get(&user_id)
                .ok_or("target device is offline")?;
            if !devices.contains_key(&target_id) {
                return Err("target device is offline");
            }
            (user_id, target_id)
        };

        self.enqueue_command_locked(
            &mut state,
            target_user_id,
            &target_id,
            command,
            payload,
            now,
        );
        Ok(())
    }

    fn enqueue_command_locked(
        &self,
        state: &mut PlayerDeviceHubState,
        user_id: i64,
        target_device_id: &str,
        command: &str,
        payload: serde_json::Value,
        now: i64,
    ) {
        let queue = state
            .commands_by_device
            .entry((user_id, target_device_id.to_string()))
            .or_default();
        while queue.len() >= PLAYER_DEVICE_MAX_COMMANDS {
            queue.pop_front();
        }
        queue.push_back(PendingPlayerDeviceCommand {
            id: uuid::Uuid::new_v4().simple().to_string(),
            command: command.to_string(),
            payload,
            created_at_ms: now,
        });
    }

    fn touch_locked(
        &self,
        state: &mut PlayerDeviceHubState,
        user_id: i64,
        device_id: &str,
        user_agent: Option<&str>,
        now: i64,
    ) {
        let devices = state.devices_by_user.entry(user_id).or_default();
        let device = PlayerDevice {
            id: device_id.to_string(),
            name: device_name_from_user_agent(user_agent),
            kind: device_kind_from_user_agent(user_agent).to_string(),
            last_seen_ms: now,
        };
        devices.insert(device_id.to_string(), device);

        let active_online = state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|active_id| devices.contains_key(active_id));
        if !active_online {
            state
                .active_device_by_user
                .insert(user_id, device_id.to_string());
        }
    }

    fn update_playback_state_locked(
        &self,
        state: &mut PlayerDeviceHubState,
        user_id: i64,
        device_id: &str,
        playback_state: Option<PlayerDevicePlaybackStateDto>,
        now: i64,
    ) {
        let is_active = state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|active_id| active_id == device_id);
        if !is_active {
            return;
        }
        let Some(mut playback_state) = playback_state else {
            return;
        };
        playback_state.updated_at_ms = now;
        state.playback_state_by_user.insert(user_id, playback_state);
        self.touch_host_jams_locked(state, user_id, device_id, now);
    }

    fn snapshot_locked(
        &self,
        state: &PlayerDeviceHubState,
        user_id: i64,
        current_device_id: &str,
        current_jam_id: Option<&str>,
        now: i64,
    ) -> PlayerDevicesResponse {
        let active_device_id = state.active_device_by_user.get(&user_id).cloned();
        let current_jam_id = current_jam_id
            .filter(|jam_id| self.jam_accessible_locked(state, user_id, jam_id, false));
        let mut devices: Vec<PlayerDeviceDto> = state
            .devices_by_user
            .get(&user_id)
            .map(|devices| {
                devices
                    .values()
                    .map(|device| PlayerDeviceDto {
                        id: device.id.clone(),
                        name: device.name.clone(),
                        kind: device.kind.clone(),
                        is_current: device.id == current_device_id,
                        is_active: active_device_id.as_deref() == Some(device.id.as_str()),
                        last_seen_ms: now.saturating_sub(device.last_seen_ms),
                    })
                    .collect()
            })
            .unwrap_or_default();
        devices.sort_by(|a, b| {
            b.is_active
                .cmp(&a.is_active)
                .then_with(|| b.is_current.cmp(&a.is_current))
                .then_with(|| a.name.cmp(&b.name))
        });
        PlayerDevicesResponse {
            device_id: current_device_id.to_string(),
            active_device_id,
            devices,
            jams: self.jam_dtos_locked(state, user_id, current_jam_id, now),
            current_jam_id: current_jam_id.map(str::to_string),
            playback_state: self.playback_state_for_context_locked(
                state,
                user_id,
                current_jam_id,
                now,
            ),
        }
    }

    fn create_jam(
        &self,
        host_user_id: i64,
        host_name: &str,
        current_device_id: &str,
        invitees: Vec<(i64, String)>,
    ) -> Result<PlayerDevicesResponse, &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);

        if self.user_has_joined_jam_locked(&state, host_user_id) {
            return Err("leave the current jam before creating a new one");
        }

        let devices = state
            .devices_by_user
            .get(&host_user_id)
            .ok_or("current device is offline")?;
        if !devices.contains_key(current_device_id) {
            return Err("current device is offline");
        }

        state
            .active_device_by_user
            .insert(host_user_id, current_device_id.to_string());

        let mut seen = HashSet::new();
        let mut members = HashMap::new();
        members.insert(
            host_user_id,
            PlayerJamMember {
                name: host_name.to_string(),
                status: PlayerJamMemberStatus::Joined,
                last_seen_ms: now,
            },
        );
        seen.insert(host_user_id);

        for (user_id, name) in invitees.into_iter().take(PLAYER_JAM_MAX_INVITEES) {
            if !seen.insert(user_id) {
                continue;
            }
            members.insert(
                user_id,
                PlayerJamMember {
                    name,
                    status: PlayerJamMemberStatus::Invited,
                    last_seen_ms: 0,
                },
            );
        }

        let jam_id = uuid::Uuid::new_v4().simple().to_string();
        let jam = PlayerJamSession {
            id: jam_id.clone(),
            host_user_id,
            host_name: host_name.to_string(),
            host_last_seen_ms: now,
            members,
        };
        state.jams_by_id.insert(jam_id.clone(), jam);
        Ok(self.snapshot_locked(&state, host_user_id, current_device_id, Some(&jam_id), now))
    }

    fn join_jam(
        &self,
        user_id: i64,
        user_name: &str,
        device_id: &str,
        jam_id: &str,
    ) -> Result<PlayerDevicesResponse, &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);

        let Some(jam) = state.jams_by_id.get_mut(jam_id) else {
            return Err("jam is not available");
        };
        let Some(member) = jam.members.get_mut(&user_id) else {
            return Err("jam is not available");
        };
        member.name = user_name.to_string();
        member.status = PlayerJamMemberStatus::Joined;
        member.last_seen_ms = now;
        if user_id == jam.host_user_id {
            jam.host_last_seen_ms = now;
        }

        Ok(self.snapshot_locked(&state, user_id, device_id, Some(jam_id), now))
    }

    fn invite_to_jam(
        &self,
        inviter_user_id: i64,
        device_id: &str,
        jam_id: &str,
        invitees: Vec<(i64, String)>,
    ) -> Result<PlayerDevicesResponse, &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);

        let Some(jam) = state.jams_by_id.get_mut(jam_id) else {
            return Err("jam is not available");
        };
        let Some(inviter) = jam.members.get(&inviter_user_id) else {
            return Err("jam is not available");
        };
        if inviter.status != PlayerJamMemberStatus::Joined {
            return Err("join the jam first");
        }
        if let Some(inviter) = jam.members.get_mut(&inviter_user_id) {
            inviter.last_seen_ms = now;
        }
        if inviter_user_id == jam.host_user_id {
            jam.host_last_seen_ms = now;
        }

        let available_slots = PLAYER_JAM_MAX_INVITEES.saturating_sub(jam.members.len());
        for (user_id, name) in invitees.into_iter().take(available_slots) {
            if user_id == inviter_user_id || jam.members.contains_key(&user_id) {
                continue;
            }
            jam.members.insert(
                user_id,
                PlayerJamMember {
                    name,
                    status: PlayerJamMemberStatus::Invited,
                    last_seen_ms: 0,
                },
            );
        }

        Ok(self.snapshot_locked(&state, inviter_user_id, device_id, Some(jam_id), now))
    }

    fn leave_jam(
        &self,
        user_id: i64,
        device_id: &str,
        jam_id: &str,
    ) -> Result<PlayerDevicesResponse, &'static str> {
        let now = current_millis();
        let mut state = self.state.lock().expect("player device hub lock");
        self.prune_locked(&mut state, now);

        let Some(jam) = state.jams_by_id.get(jam_id) else {
            return Ok(self.snapshot_locked(&state, user_id, device_id, None, now));
        };
        if !jam.members.contains_key(&user_id) {
            return Err("jam is not available");
        }
        if jam.host_user_id == user_id {
            state.jams_by_id.remove(jam_id);
        } else if let Some(jam) = state.jams_by_id.get_mut(jam_id) {
            jam.members.remove(&user_id);
        }

        Ok(self.snapshot_locked(&state, user_id, device_id, None, now))
    }

    fn touch_jam_locked(
        &self,
        state: &mut PlayerDeviceHubState,
        user_id: i64,
        device_id: &str,
        current_jam_id: Option<&str>,
        now: i64,
    ) {
        let Some(jam_id) = current_jam_id else {
            return;
        };
        let is_active_host_device = state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|active_id| active_id == device_id);
        let Some(jam) = state.jams_by_id.get_mut(jam_id) else {
            return;
        };
        let Some(member) = jam.members.get_mut(&user_id) else {
            return;
        };
        member.last_seen_ms = now;
        if member.status == PlayerJamMemberStatus::Invited {
            return;
        }
        if user_id == jam.host_user_id && is_active_host_device {
            jam.host_last_seen_ms = now;
        }
    }

    fn touch_host_jams_locked(
        &self,
        state: &mut PlayerDeviceHubState,
        user_id: i64,
        device_id: &str,
        now: i64,
    ) {
        let is_active = state
            .active_device_by_user
            .get(&user_id)
            .is_some_and(|active_id| active_id == device_id);
        if !is_active {
            return;
        }
        for jam in state.jams_by_id.values_mut() {
            if jam.host_user_id == user_id {
                jam.host_last_seen_ms = now;
                if let Some(member) = jam.members.get_mut(&user_id) {
                    member.last_seen_ms = now;
                }
            }
        }
    }

    fn jam_accessible_locked(
        &self,
        state: &PlayerDeviceHubState,
        user_id: i64,
        jam_id: &str,
        require_joined: bool,
    ) -> bool {
        let Some(jam) = state.jams_by_id.get(jam_id) else {
            return false;
        };
        let Some(member) = jam.members.get(&user_id) else {
            return false;
        };
        !require_joined || member.status == PlayerJamMemberStatus::Joined
    }

    fn user_has_joined_jam_locked(&self, state: &PlayerDeviceHubState, user_id: i64) -> bool {
        state.jams_by_id.values().any(|jam| {
            jam.members
                .get(&user_id)
                .is_some_and(|member| member.status == PlayerJamMemberStatus::Joined)
        })
    }

    fn jam_target_device_id_locked(
        &self,
        state: &PlayerDeviceHubState,
        jam: &PlayerJamSession,
    ) -> Option<String> {
        let active_device_id = state.active_device_by_user.get(&jam.host_user_id)?;
        let host_devices = state.devices_by_user.get(&jam.host_user_id)?;
        host_devices
            .contains_key(active_device_id)
            .then(|| active_device_id.clone())
    }

    fn playback_state_for_context_locked(
        &self,
        state: &PlayerDeviceHubState,
        user_id: i64,
        current_jam_id: Option<&str>,
        now: i64,
    ) -> Option<PlayerDevicePlaybackStateDto> {
        let playback_user_id = current_jam_id
            .and_then(|jam_id| state.jams_by_id.get(jam_id))
            .and_then(|jam| {
                jam.members.get(&user_id).and_then(|member| {
                    (member.status == PlayerJamMemberStatus::Joined).then_some(jam.host_user_id)
                })
            })
            .unwrap_or(user_id);
        state
            .playback_state_by_user
            .get(&playback_user_id)
            .cloned()
            .map(|playback_state| playback_state_at(playback_state, now))
    }

    fn jam_dtos_locked(
        &self,
        state: &PlayerDeviceHubState,
        user_id: i64,
        current_jam_id: Option<&str>,
        now: i64,
    ) -> Vec<PlayerJamDto> {
        let mut jams: Vec<PlayerJamDto> = state
            .jams_by_id
            .values()
            .filter_map(|jam| {
                let member = jam.members.get(&user_id)?;
                let member_count = jam
                    .members
                    .values()
                    .filter(|member| member.status == PlayerJamMemberStatus::Joined)
                    .count() as i64;
                let mut members = jam
                    .members
                    .iter()
                    .map(|(member_user_id, member)| PlayerJamMemberDto {
                        user_id: *member_user_id,
                        name: member.name.clone(),
                        is_joined: member.status == PlayerJamMemberStatus::Joined,
                        is_current_user: *member_user_id == user_id,
                        last_seen_ms: now.saturating_sub(member.last_seen_ms),
                    })
                    .collect::<Vec<_>>();
                members.sort_by(|a, b| {
                    b.is_joined
                        .cmp(&a.is_joined)
                        .then_with(|| b.is_current_user.cmp(&a.is_current_user))
                        .then_with(|| a.name.cmp(&b.name))
                });
                let host_device_online = self.jam_target_device_id_locked(state, jam).is_some();
                Some(PlayerJamDto {
                    id: jam.id.clone(),
                    name: format!("{}'s Jam", jam.host_name),
                    host_user_id: jam.host_user_id,
                    host_name: jam.host_name.clone(),
                    is_owner: jam.host_user_id == user_id,
                    is_member: member.status == PlayerJamMemberStatus::Joined,
                    is_pending: member.status == PlayerJamMemberStatus::Invited,
                    is_active: current_jam_id == Some(jam.id.as_str()),
                    member_count,
                    host_last_seen_ms: now.saturating_sub(jam.host_last_seen_ms),
                    host_device_online,
                    members,
                })
            })
            .collect();
        jams.sort_by(|a, b| {
            b.is_active
                .cmp(&a.is_active)
                .then_with(|| b.is_owner.cmp(&a.is_owner))
                .then_with(|| b.is_pending.cmp(&a.is_pending))
                .then_with(|| a.name.cmp(&b.name))
        });
        jams
    }

    fn prune_locked(&self, state: &mut PlayerDeviceHubState, now: i64) {
        state
            .jams_by_id
            .retain(|_, jam| now.saturating_sub(jam.host_last_seen_ms) <= PLAYER_JAM_IDLE_TTL_MS);

        state.devices_by_user.retain(|user_id, devices| {
            devices.retain(|_, device| {
                now.saturating_sub(device.last_seen_ms) <= PLAYER_DEVICE_TTL_MS
            });
            let active_valid = state
                .active_device_by_user
                .get(user_id)
                .is_some_and(|active_id| devices.contains_key(active_id));
            if !active_valid {
                if let Some(first_device_id) = devices.keys().next().cloned() {
                    state
                        .active_device_by_user
                        .insert(*user_id, first_device_id);
                } else {
                    state.active_device_by_user.remove(user_id);
                    state.playback_state_by_user.remove(user_id);
                }
            }
            !devices.is_empty()
        });
        state
            .playback_state_by_user
            .retain(|user_id, _| state.devices_by_user.contains_key(user_id));

        state
            .commands_by_device
            .retain(|(user_id, device_id), queue| {
                let device_online = state
                    .devices_by_user
                    .get(user_id)
                    .is_some_and(|devices| devices.contains_key(device_id));
                if !device_online {
                    return false;
                }
                queue.retain(|cmd| {
                    now.saturating_sub(cmd.created_at_ms) <= PLAYER_DEVICE_COMMAND_TTL_MS
                });
                !queue.is_empty()
            });
    }
}

fn current_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fed_virtual_device_id(device_id: &str) -> String {
    format!("{FED_DEVICE_PREFIX}{device_id}")
}

fn is_fed_virtual_device_id(device_id: &str) -> bool {
    device_id.starts_with(FED_DEVICE_PREFIX)
}

fn fed_device_id_from_virtual(device_id: &str) -> Option<&str> {
    device_id.strip_prefix(FED_DEVICE_PREFIX)
}

fn playback_state_at(
    mut playback_state: PlayerDevicePlaybackStateDto,
    now: i64,
) -> PlayerDevicePlaybackStateDto {
    if !playback_state.paused && playback_state.updated_at_ms > 0 {
        let elapsed_seconds = now.saturating_sub(playback_state.updated_at_ms) as f64 / 1000.0;
        playback_state.position_seconds += elapsed_seconds;
        if playback_state.duration_seconds > 0.0 {
            playback_state.position_seconds = playback_state
                .position_seconds
                .min(playback_state.duration_seconds);
        }
    }
    playback_state.updated_at_ms = now;
    playback_state
}

fn normalize_device_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return None;
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':')
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn device_name_from_user_agent(user_agent: Option<&str>) -> String {
    if let Some(name) = native_device_name_from_user_agent(user_agent) {
        return name;
    }

    let ua = user_agent.unwrap_or_default().to_ascii_lowercase();
    let browser = if ua.contains("edg/") || ua.contains("edgios/") || ua.contains("edga/") {
        "Edge"
    } else if ua.contains("firefox/") || ua.contains("fxios/") {
        "Firefox"
    } else if ua.contains("opr/") || ua.contains("opera") {
        "Opera"
    } else if ua.contains("chrome/") || ua.contains("crios/") {
        "Chrome"
    } else if ua.contains("safari/") {
        "Safari"
    } else {
        "Browser"
    };

    let os = if ua.contains("iphone") {
        "iPhone"
    } else if ua.contains("ipad") {
        "iPad"
    } else if ua.contains("android") {
        "Android"
    } else if ua.contains("windows") {
        "Windows"
    } else if ua.contains("mac os") || ua.contains("macintosh") {
        "macOS"
    } else if ua.contains("linux") {
        "Linux"
    } else {
        "Device"
    };

    format!("{browser} on {os}")
}

fn native_device_name_from_user_agent(user_agent: Option<&str>) -> Option<String> {
    let raw = user_agent?.trim();
    for token in raw.split_ascii_whitespace() {
        let Some((product, version)) = token.split_once('/') else {
            continue;
        };
        let version = sanitize_user_agent_version(version);
        if product.eq_ignore_ascii_case("FurumiAndroid") {
            return Some(match version.as_deref() {
                Some(v) => format!("Furumi Android {v}"),
                None => "Furumi Android".to_string(),
            });
        }
        if product.eq_ignore_ascii_case("FurumiMacOS") {
            return Some(match version.as_deref() {
                Some(v) => format!("Furumi MacOS {v}"),
                None => "Furumi MacOS".to_string(),
            });
        }
        if product.eq_ignore_ascii_case("FurumiTUI") || product.eq_ignore_ascii_case("furumi-tui") {
            return Some(match version.as_deref() {
                Some(v) => format!("Furumi TUI {v}"),
                None => "Furumi TUI".to_string(),
            });
        }
    }
    None
}

fn sanitize_user_agent_version(version: &str) -> Option<String> {
    let version = version
        .chars()
        .take(32)
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        .collect::<String>();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn device_kind_from_user_agent(user_agent: Option<&str>) -> &'static str {
    let ua = user_agent.unwrap_or_default().to_ascii_lowercase();
    if ua.contains("furumiandroid/") {
        return if ua.contains("tablet") || (ua.contains("android") && !ua.contains("mobile")) {
            "tablet"
        } else {
            "phone"
        };
    }
    if ua.contains("furumimac") {
        return "computer";
    }
    if ua.contains("furumitui/") || ua.contains("furumi-tui/") {
        return "computer";
    }
    if ua.contains("iphone") || (ua.contains("android") && ua.contains("mobile")) {
        "phone"
    } else if ua.contains("ipad") || ua.contains("tablet") || ua.contains("android") {
        "tablet"
    } else {
        "computer"
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn detects_furumi_android_native_client() {
        let user_agent = Some("FurumiAndroid/1.0 Android Mobile");

        assert_eq!(
            device_name_from_user_agent(user_agent),
            "Furumi Android 1.0"
        );
        assert_eq!(device_kind_from_user_agent(user_agent), "phone");
    }

    #[test]
    fn detects_furumi_tui_native_client() {
        let user_agent = Some("FurumiTUI/0.1.0 macos");

        assert_eq!(device_name_from_user_agent(user_agent), "Furumi TUI 0.1.0");
        assert_eq!(device_kind_from_user_agent(user_agent), "computer");
    }

    #[test]
    fn detects_furumi_tui_http_user_agent_token() {
        let user_agent = Some("furumi-tui/0.1.0 (macos)");

        assert_eq!(device_name_from_user_agent(user_agent), "Furumi TUI 0.1.0");
        assert_eq!(device_kind_from_user_agent(user_agent), "computer");
    }

    #[test]
    fn keeps_browser_fallback_for_generic_android_user_agents() {
        let user_agent = Some("Mozilla/5.0 Android Mobile");

        assert_eq!(
            device_name_from_user_agent(user_agent),
            "Browser on Android"
        );
        assert_eq!(device_kind_from_user_agent(user_agent), "phone");
    }

    #[test]
    fn accepts_virtual_fed_device_ids() {
        assert_eq!(
            normalize_device_id("fed:dev_e5ffc3b65642770c26c53ecf"),
            Some("fed:dev_e5ffc3b65642770c26c53ecf".to_string())
        );
    }

    #[test]
    fn federated_snapshot_does_not_steal_active_browser_playback() {
        let hub = PlayerDeviceHub::default();
        let user_id = 7;
        {
            let mut state = hub.state.lock().expect("device hub");
            state.devices_by_user.entry(user_id).or_default().insert(
                "browser".to_string(),
                PlayerDevice {
                    id: "browser".to_string(),
                    name: "Browser".to_string(),
                    kind: "computer".to_string(),
                    last_seen_ms: current_millis(),
                },
            );
            state
                .active_device_by_user
                .insert(user_id, "browser".to_string());
            state.playback_state_by_user.insert(
                user_id,
                PlayerDevicePlaybackStateDto {
                    track: Some(serde_json::json!({"id": 1})),
                    tracks: vec![],
                    index: 0,
                    position_seconds: 10.0,
                    duration_seconds: 100.0,
                    paused: false,
                    shuffle: false,
                    repeat_mode: "off".to_string(),
                    volume: 0.7,
                    updated_at_ms: current_millis(),
                },
            );
        }

        hub.apply_fed_playback_state_json(
            user_id,
            "remote",
            "Remote",
            true,
            serde_json::json!({
                "track": {"id": 2},
                "tracks": [],
                "index": 0,
                "position_seconds": 0.0,
                "duration_seconds": 100.0,
                "paused": false,
                "shuffle": false,
                "repeat_mode": "off",
                "volume": 0.7
            }),
        )
        .expect("valid snapshot");

        let state = hub.state.lock().expect("device hub");
        assert_eq!(
            state
                .active_device_by_user
                .get(&user_id)
                .map(String::as_str),
            Some("browser")
        );
        assert!(
            state
                .devices_by_user
                .get(&user_id)
                .is_some_and(|devices| devices.contains_key("fed:remote"))
        );
    }
}

#[derive(Debug, sqlx::FromRow)]
struct LastfmAccountApiRow {
    session_key: String,
    reauth_required: bool,
    last_error: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct LastfmStatusRow {
    username: String,
    reauth_required: bool,
    last_error: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct LastfmTrackMetaRow {
    title: String,
    duration_seconds: f64,
    track_number: Option<i32>,
    album_title: Option<String>,
    artist_name: Option<String>,
    album_artist_name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LastfmCallbackQuery {
    token: Option<String>,
    state: Option<String>,
}

// ---------------------------------------------------------------------------
// SPA shell
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "player.html")]
pub struct PlayerPageTemplate {
    pub t: &'static Translations,
}

// ---------------------------------------------------------------------------
// GET /api/player/me
// ---------------------------------------------------------------------------

async fn me_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let liked_tracks: (i64,) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM furumusic__user_liked_track WHERE user_id = $1)
          + (SELECT COUNT(*) FROM furumusic__fed_state_like
              WHERE user_id = $1 AND liked = true AND local_track_id IS NULL)",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let playlists: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM furumusic__playlist WHERE owner_id = $1")
            .bind(user.id)
            .fetch_one(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;

    let plays: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM furumusic__listen_event
          WHERE user_id = $1 AND qualified = true",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let listened_seconds: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(listened_ms), 0) / 1000
           FROM furumusic__listen_event
          WHERE user_id = $1 AND qualified = true",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(UserProfile {
        id: user.id,
        name: user.name,
        role: user.role.code().to_string(),
        stats: UserStats {
            liked_tracks: liked_tracks.0,
            playlists: playlists.0,
            plays: plays.0,
            listened_minutes: listened_seconds.unwrap_or(0) / 60,
        },
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/offline/manifest
// ---------------------------------------------------------------------------

async fn offline_manifest_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let generated_at = now_iso_string();

    let track_rows = sqlx::query_as::<_, OfflineTrackManifestRow>(
        r#"SELECT t.id,
                  GREATEST(
                      t.updated_at::text,
                      r.updated_at::text,
                      mf.created_at::text,
                      COALESCE(cover_mf.created_at::text, ''),
                      COALESCE((
                          SELECT MAX(a.updated_at::text)
                            FROM furumusic__track_artist ta
                            JOIN furumusic__artist a ON a.id = ta.artist_id
                           WHERE ta.track_id = t.id
                      ), ''),
                      COALESCE((
                          SELECT MAX(a.updated_at::text)
                            FROM furumusic__release_artist ra
                            JOIN furumusic__artist a ON a.id = ra.artist_id
                           WHERE ra.release_id = r.id
                      ), '')
                  ) AS updated_at,
                  mf.id AS audio_file_id,
                  mf.sha256_hash::text AS audio_hash,
                  mf.file_size_bytes AS audio_size_bytes,
                  mf.mime_type::text AS audio_mime_type,
                  mf.created_at::text AS audio_updated_at,
                  cover_mf.id AS cover_file_id,
                  cover_mf.sha256_hash::text AS cover_hash,
                  cover_mf.created_at::text AS cover_updated_at
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           LEFT JOIN furumusic__media_file cover_mf
                  ON cover_mf.id = COALESCE(t.cover_file_id, r.cover_file_id)
           WHERE t.is_hidden = false AND r.is_hidden = false
           ORDER BY t.id"#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let liked_track_ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT ult.track_id
           FROM furumusic__user_liked_track ult
           JOIN furumusic__track t ON t.id = ult.track_id AND t.is_hidden = false
           JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
           WHERE ult.user_id = $1
           ORDER BY ult.created_at DESC"#,
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let likes_updated_at = sqlx::query_scalar::<_, Option<String>>(
        "SELECT MAX(created_at::text) FROM furumusic__user_liked_track WHERE user_id = $1",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?
    .unwrap_or_else(|| generated_at.clone());

    let playlist_rows = sqlx::query_as::<_, OfflinePlaylistManifestRow>(
        r#"SELECT p.id,
                  p.title::text AS title,
                  p.description,
                  p.updated_at::text AS updated_at,
                  (p.owner_id = $1) AS is_own,
                  COALESCE(NULLIF(u.display_name, ''), u.username)::text AS owner_name,
                  p.is_public,
                  EXISTS (
                      SELECT 1 FROM furumusic__saved_playlist sp
                       WHERE sp.user_id = $1 AND sp.playlist_id = p.id
                  ) AS is_saved,
                  COALESCE(
                      array_agg(pt.track_id ORDER BY pt.position)
                          FILTER (WHERE t.id IS NOT NULL AND r.id IS NOT NULL),
                      ARRAY[]::bigint[]
                  ) AS track_ids
           FROM furumusic__playlist p
           JOIN furumusic__user u ON u.id = p.owner_id
           LEFT JOIN furumusic__playlist_track pt ON pt.playlist_id = p.id
           LEFT JOIN furumusic__track t ON t.id = pt.track_id AND t.is_hidden = false
           LEFT JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
           WHERE p.owner_id = $1
              OR EXISTS (
                  SELECT 1 FROM furumusic__saved_playlist sp
                   WHERE sp.user_id = $1 AND sp.playlist_id = p.id
              )
              OR p.is_public = true
           GROUP BY p.id, u.display_name, u.username
           ORDER BY
              CASE WHEN p.owner_id = $1 THEN 0 WHEN p.is_public THEN 2 ELSE 1 END,
              p.title"#,
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let followed_artist_ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT ufa.artist_id
           FROM furumusic__user_followed_artist ufa
           JOIN furumusic__artist a ON a.id = ufa.artist_id AND a.is_hidden = false
           WHERE ufa.user_id = $1
           ORDER BY ufa.created_at DESC"#,
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let tracks = track_rows
        .into_iter()
        .map(|row| OfflineTrackManifestItem {
            id: row.id,
            updated_at: row.updated_at,
            stream_url: format!("/api/player/stream/{}", row.id),
            audio_file_id: row.audio_file_id,
            audio_hash: row.audio_hash,
            audio_size_bytes: row.audio_size_bytes,
            audio_mime_type: row.audio_mime_type,
            audio_updated_at: row.audio_updated_at,
            cover_file_id: row.cover_file_id,
            cover_url: cover_variant_url(row.cover_file_id, "medium"),
            cover_hash: row.cover_hash,
            cover_updated_at: row.cover_updated_at,
        })
        .collect();

    let mut playlists = vec![OfflinePlaylistManifestItem {
        id: -1,
        title: "Likes".to_string(),
        description: None,
        updated_at: likes_updated_at,
        is_own: true,
        owner_name: None,
        is_public: false,
        is_saved: false,
        kind: "likes".to_string(),
        track_ids: liked_track_ids.clone(),
    }];

    playlists.extend(
        playlist_rows
            .into_iter()
            .map(|row| OfflinePlaylistManifestItem {
                id: row.id,
                title: row.title,
                description: row.description,
                updated_at: row.updated_at,
                is_own: row.is_own,
                owner_name: Some(row.owner_name),
                is_public: row.is_public,
                is_saved: row.is_saved,
                kind: "user".to_string(),
                track_ids: row.track_ids,
            }),
    );

    Json(OfflineManifestResponse {
        generated_at,
        tracks,
        playlists,
        liked_track_ids,
        followed_artist_ids,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// Last.fm account + scrobbling
// ---------------------------------------------------------------------------

fn redirect_response(location: &str) -> cot::response::Response {
    cot::http::Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(cot::http::header::LOCATION, location)
        .body(Body::fixed(""))
        .expect("valid response")
}

fn request_origin(request: &cot::request::Request) -> Option<String> {
    let headers = request.headers();
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?;
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

async fn lastfm_status_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    let configured = crate::lastfm::is_configured(&config);
    let account = sqlx::query_as::<_, LastfmStatusRow>(
        r#"SELECT lastfm_username::text AS username,
                  reauth_required,
                  last_error::text AS last_error
             FROM furumusic__lastfm_account
            WHERE user_id = $1"#,
    )
    .bind(user.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(LastfmStatus {
        configured,
        connected: account.is_some(),
        username: account.as_ref().map(|row| row.username.clone()),
        reauth_required: account
            .as_ref()
            .map(|row| row.reauth_required)
            .unwrap_or(false),
        last_error: account.and_then(|row| row.last_error),
    })
    .into_response()
}

async fn lastfm_connect_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    request: cot::request::Request,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(redirect_response("/login"));
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    let Some(credentials) = LastfmCredentials::from_config(&config) else {
        return Ok(redirect_response("/"));
    };
    let Some(origin) = request_origin(&request) else {
        return Ok(redirect_response("/"));
    };

    let state = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now();
    let stale = (now - chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    sqlx::query("DELETE FROM furumusic__lastfm_auth_state WHERE created_at < $1")
        .bind(stale)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query(
        r#"INSERT INTO furumusic__lastfm_auth_state (state, user_id, created_at)
           VALUES ($1, $2, $3)
           ON CONFLICT (state) DO NOTHING"#,
    )
    .bind(&state)
    .bind(user.id)
    .bind(now.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let callback = format!("{origin}/api/player/lastfm/callback?state={state}");
    let mut url = reqwest::Url::parse("https://www.last.fm/api/auth/")
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("api_key", credentials.api_key())
        .append_pair("cb", &callback);
    Ok(redirect_response(url.as_str()))
}

async fn lastfm_callback_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    query: cot::request::extractors::UrlQuery<LastfmCallbackQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(redirect_response("/login"));
    };
    let Some(token) = query
        .0
        .token
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Ok(redirect_response("/"));
    };
    let Some(state) = query
        .0
        .state
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Ok(redirect_response("/"));
    };

    let state_user_id = sqlx::query_scalar::<_, i64>(
        "SELECT user_id FROM furumusic__lastfm_auth_state WHERE state = $1",
    )
    .bind(state)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if state_user_id != Some(user.id) {
        return Ok(redirect_response("/"));
    }
    sqlx::query("DELETE FROM furumusic__lastfm_auth_state WHERE state = $1")
        .bind(state)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let (config, _) = AppConfig::load_with_db(&db).await;
    let Some(credentials) = LastfmCredentials::from_config(&config) else {
        return Ok(redirect_response("/"));
    };
    let client = LastfmClient::new(credentials).map_err(|e| cot::Error::internal(e.to_string()))?;
    match client.get_session(token).await {
        Ok(lastfm_session) => {
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            sqlx::query(
                r#"INSERT INTO furumusic__lastfm_account
                      (user_id, lastfm_username, session_key, connected_at, updated_at, last_error, reauth_required)
                   VALUES ($1, $2, $3, $4, $4, NULL, false)
                   ON CONFLICT (user_id) DO UPDATE SET
                      lastfm_username = EXCLUDED.lastfm_username,
                      session_key = EXCLUDED.session_key,
                      updated_at = EXCLUDED.updated_at,
                      last_error = NULL,
                      reauth_required = false"#,
            )
            .bind(user.id)
            .bind(&lastfm_session.username)
            .bind(&lastfm_session.session_key)
            .bind(&now)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
            Ok(redirect_response("/"))
        }
        Err(err) => {
            tracing::warn!("Last.fm auth failed for user {}: {err}", user.id);
            Ok(redirect_response("/"))
        }
    }
}

async fn lastfm_disconnect_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query("DELETE FROM furumusic__lastfm_account WHERE user_id = $1")
        .bind(user.id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query(
        r#"UPDATE furumusic__lastfm_scrobble_outbox
              SET status = 'blocked',
                  last_error = 'Last.fm account disconnected',
                  updated_at = $2
            WHERE user_id = $1 AND status IN ('pending', 'retry')"#,
    )
    .bind(user.id)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn load_lastfm_account(
    pool: &sqlx::PgPool,
    user_id: i64,
) -> cot::Result<Option<LastfmAccountApiRow>> {
    sqlx::query_as::<_, LastfmAccountApiRow>(
        r#"SELECT session_key::text AS session_key,
                  reauth_required,
                  last_error::text AS last_error
             FROM furumusic__lastfm_account
            WHERE user_id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn load_lastfm_track_payload(
    pool: &sqlx::PgPool,
    track_id: i64,
) -> cot::Result<Option<LastfmTrackPayload>> {
    let row = sqlx::query_as::<_, LastfmTrackMetaRow>(
        r#"SELECT t.title::text AS title,
                  t.duration_seconds,
                  t.track_number,
                  r.title::text AS album_title,
                  (
                    SELECT a.name::text
                      FROM furumusic__track_artist ta
                      JOIN furumusic__artist a ON a.id = ta.artist_id
                     WHERE ta.track_id = t.id AND ta.role <> 'featuring'
                     ORDER BY ta.position
                     LIMIT 1
                  ) AS artist_name,
                  (
                    SELECT a.name::text
                      FROM furumusic__release_artist ra
                      JOIN furumusic__artist a ON a.id = ra.artist_id
                     WHERE ra.release_id = r.id
                     ORDER BY ra.position
                     LIMIT 1
                  ) AS album_artist_name
             FROM furumusic__track t
             LEFT JOIN furumusic__release r ON r.id = t.release_id
            WHERE t.id = $1 AND t.is_hidden = false"#,
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Ok(row.and_then(|row| {
        let artist = row
            .artist_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?
            .to_string();
        Some(LastfmTrackPayload {
            artist,
            track: row.title,
            album: row
                .album_title
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            album_artist: row
                .album_artist_name
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            track_number: row.track_number,
            duration_seconds: Some(row.duration_seconds.round() as i32),
        })
    }))
}

async fn update_lastfm_account_error(
    pool: &sqlx::PgPool,
    user_id: i64,
    error: &str,
    reauth_required: bool,
) -> cot::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__lastfm_account
              SET last_error = $2,
                  reauth_required = $3,
                  updated_at = $4
            WHERE user_id = $1"#,
    )
    .bind(user_id)
    .bind(error)
    .bind(reauth_required)
    .bind(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string())
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(())
}

async fn enqueue_lastfm_scrobble(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    user_id: i64,
    track_id: i64,
    started_at: Option<i64>,
    listened_seconds: i32,
) -> cot::Result<LastfmActionResponse> {
    if !crate::lastfm::is_configured(config) {
        return Ok(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Last.fm is not configured".to_string()),
        });
    }
    if load_lastfm_account(pool, user_id).await?.is_none() {
        return Ok(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Last.fm account is not connected".to_string()),
        });
    }
    let Some(track) = load_lastfm_track_payload(pool, track_id).await? else {
        return Ok(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Track has no primary artist for Last.fm".to_string()),
        });
    };
    let duration_seconds = track.duration_seconds.unwrap_or(0).max(0);
    if duration_seconds <= 30 {
        return Ok(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Track is too short to scrobble".to_string()),
        });
    }
    let threshold = ((duration_seconds as f64 / 2.0).min(240.0)).ceil() as i32;
    let listened_seconds = listened_seconds.max(0);
    if listened_seconds < threshold {
        return Ok(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Scrobble threshold has not been reached".to_string()),
        });
    }

    let now_ts = chrono::Utc::now().timestamp();
    let started_at = started_at
        .unwrap_or(now_ts - listened_seconds as i64)
        .min(now_ts);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let dedupe_key = format!("{user_id}:{track_id}:{started_at}");
    sqlx::query(
        r#"INSERT INTO furumusic__lastfm_scrobble_outbox
              (user_id, track_id, started_at, listened_seconds, duration_seconds, status, created_at, updated_at, dedupe_key)
           VALUES ($1, $2, $3, $4, $5, 'pending', $6, $6, $7)
           ON CONFLICT (dedupe_key) DO NOTHING"#,
    )
    .bind(user_id)
    .bind(track_id)
    .bind(started_at)
    .bind(listened_seconds)
    .bind(duration_seconds)
    .bind(&now)
    .bind(&dedupe_key)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let sent = match crate::lastfm::process_pending_scrobbles(pool, config, Some(user_id), 10).await
    {
        Ok(summary) => summary.sent > 0,
        Err(err) => {
            tracing::warn!("Last.fm immediate scrobble send failed: {err:#}");
            false
        }
    };

    Ok(LastfmActionResponse {
        ok: true,
        queued: true,
        sent,
        message: None,
    })
}

async fn lastfm_now_playing_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(entry): Json<LastfmNowPlayingRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    let Some(credentials) = LastfmCredentials::from_config(&config) else {
        return Json(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Last.fm is not configured".to_string()),
        })
        .into_response();
    };
    let Some(account) = load_lastfm_account(pool, user.id).await? else {
        return Json(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Last.fm account is not connected".to_string()),
        })
        .into_response();
    };
    if account.reauth_required {
        return Json(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: account.last_error,
        })
        .into_response();
    }
    let Some(track) = load_lastfm_track_payload(pool, entry.track_id).await? else {
        return Json(LastfmActionResponse {
            ok: false,
            queued: false,
            sent: false,
            message: Some("Track has no primary artist for Last.fm".to_string()),
        })
        .into_response();
    };
    let client = LastfmClient::new(credentials).map_err(|e| cot::Error::internal(e.to_string()))?;
    match client
        .update_now_playing(&account.session_key, &track)
        .await
    {
        Ok(()) => Json(LastfmActionResponse {
            ok: true,
            queued: false,
            sent: true,
            message: None,
        })
        .into_response(),
        Err(err) => {
            let reauth_required = err.is_invalid_session();
            update_lastfm_account_error(pool, user.id, &err.to_string(), reauth_required).await?;
            Json(LastfmActionResponse {
                ok: false,
                queued: false,
                sent: false,
                message: Some(err.to_string()),
            })
            .into_response()
        }
    }
}

async fn lastfm_scrobble_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(entry): Json<LastfmScrobbleRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    Json(
        enqueue_lastfm_scrobble(
            pool,
            &config,
            user.id,
            entry.track_id,
            entry.started_at,
            entry.listened_seconds,
        )
        .await?,
    )
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/agent-queue
// ---------------------------------------------------------------------------

async fn agent_queue_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let (queued_count, processing_count): (i64, i64) = sqlx::query_as(
        r#"SELECT
              COUNT(*) FILTER (WHERE status = 'queued') AS queued_count,
              COUNT(*) FILTER (WHERE status = 'processing') AS processing_count
           FROM furumusic__pending_review"#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(AgentQueueStatus {
        queued_count,
        processing_count,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// User-uploaded tracks
// ---------------------------------------------------------------------------

async fn user_uploads_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    UrlQuery(query): UrlQuery<UserUploadsQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let limit = query.limit.unwrap_or(120).clamp(1, 500);
    let page = load_user_uploads_page(pool, user.id, limit as i64).await?;
    Json(page).into_response()
}

async fn user_upload_track_update_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathTrackId>,
    Json(body): Json<UserUploadTrackUpdateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let track_id = path.0.track_id;
    let Some(existing) = sqlx::query_as::<_, UploadTrackEditRow>(
        r#"SELECT t.release_id,
                  t.title::text AS title,
                  t.track_number,
                  t.disc_number,
                  t.is_hidden,
                  r.title::text AS release_title,
                  r.release_type::text AS release_type,
                  r.year AS release_year
           FROM furumusic__track t
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           JOIN furumusic__release r ON r.id = t.release_id
           WHERE t.id = $1 AND mf.uploaded_by_user_id = $2"#,
    )
    .bind(track_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?
    else {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "uploaded track not found",
        ));
    };

    let title = match clean_required_string(body.title.as_deref(), &existing.title, 255) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let release_title =
        match clean_required_string(body.release_title.as_deref(), &existing.release_title, 255) {
            Ok(value) => value,
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        };
    let release_type = body
        .release_type
        .as_deref()
        .map(normalize_release_type)
        .unwrap_or_else(|| existing.release_type.clone());
    let release_year = match parse_optional_i32(
        body.release_year.as_deref(),
        existing.release_year,
        0,
        3000,
        "invalid release year",
    ) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let track_number = match parse_optional_i32(
        body.track_number.as_deref(),
        existing.track_number,
        1,
        999,
        "invalid track number",
    ) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let disc_number = match parse_optional_i32(
        body.disc_number.as_deref(),
        existing.disc_number,
        1,
        99,
        "invalid disc number",
    ) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let is_hidden = body.is_hidden.unwrap_or(existing.is_hidden);

    let release_changed = release_title != existing.release_title
        || release_type != existing.release_type
        || release_year != existing.release_year;
    if release_changed && !user_owns_release_tracks(pool, user.id, existing.release_id).await? {
        return Ok(json_error(
            StatusCode::FORBIDDEN,
            "release contains tracks uploaded by another user",
        ));
    }

    let artist_names = match body.artist_names {
        Some(names) => match clean_artist_names(names) {
            Ok(names) => Some(names),
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        },
        None => None,
    };
    let featured_artist_names = match body.featured_artist_names {
        Some(names) => match clean_optional_artist_names(names) {
            Ok(names) => Some(names),
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        },
        None => None,
    };

    let now = now_iso_string();
    if release_changed {
        sqlx::query(
            r#"UPDATE furumusic__release
               SET title = $1, title_sort = $2, release_type = $3, year = $4, updated_at = $5
               WHERE id = $6"#,
        )
        .bind(&release_title)
        .bind(sort_name(&release_title))
        .bind(&release_type)
        .bind(release_year)
        .bind(&now)
        .bind(existing.release_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }

    sqlx::query(
        r#"UPDATE furumusic__track
           SET title = $1,
               title_sort = $2,
               track_number = $3,
               disc_number = $4,
               year = $5,
               is_hidden = $6,
               updated_at = $7
           WHERE id = $8"#,
    )
    .bind(&title)
    .bind(sort_name(&title))
    .bind(track_number)
    .bind(disc_number)
    .bind(release_year)
    .bind(is_hidden)
    .bind(&now)
    .bind(track_id)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if let Some(artist_names) = artist_names {
        sqlx::query("DELETE FROM furumusic__track_artist WHERE track_id = $1 AND role = 'main'")
            .bind(track_id)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        for (position, name) in artist_names.iter().enumerate() {
            let artist_id = find_or_create_player_artist(pool, name).await?;
            sqlx::query(
                r#"INSERT INTO furumusic__track_artist (track_id, artist_id, role, position)
                   VALUES ($1, $2, 'main', $3)"#,
            )
            .bind(track_id)
            .bind(artist_id)
            .bind(position as i32)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        }
    }
    if let Some(featured_artist_names) = featured_artist_names {
        replace_track_role_artists(pool, track_id, "featuring", &featured_artist_names, 1).await?;
    }

    let mut tracks = load_user_upload_tracks(pool, user.id, Some(track_id), 1).await?;
    let Some(track) = tracks.pop() else {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "uploaded track not found",
        ));
    };
    Json(track).into_response()
}

async fn user_upload_release_update_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<UserUploadReleaseUpdateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let release_id = path.0.id;
    if !user_owns_release_tracks(pool, user.id, release_id).await? {
        return Ok(json_error(
            StatusCode::FORBIDDEN,
            "release contains tracks uploaded by another user",
        ));
    }

    let Some(existing) = sqlx::query_as::<_, UploadTrackEditRow>(
        r#"SELECT t.release_id,
                  t.title::text AS title,
                  t.track_number,
                  t.disc_number AS disc_number,
                  t.is_hidden,
                  r.title::text AS release_title,
                  r.release_type::text AS release_type,
                  r.year AS release_year
           FROM furumusic__release r
           JOIN furumusic__track t ON t.release_id = r.id
           WHERE r.id = $1
           ORDER BY t.id
           LIMIT 1"#,
    )
    .bind(release_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?
    else {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "uploaded release not found",
        ));
    };

    let title = match clean_required_string(body.title.as_deref(), &existing.release_title, 255) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let release_type = body
        .release_type
        .as_deref()
        .map(normalize_release_type)
        .unwrap_or(existing.release_type);
    let year = match parse_optional_i32(
        body.year.as_deref(),
        existing.release_year,
        0,
        3000,
        "invalid release year",
    ) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let artist_names = match body.artist_names {
        Some(names) => match clean_artist_names(names) {
            Ok(names) => Some(names),
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        },
        None => None,
    };
    let now = now_iso_string();
    sqlx::query(
        r#"UPDATE furumusic__release
           SET title = $1,
               title_sort = $2,
               release_type = $3,
               year = $4,
               is_hidden = COALESCE($5, is_hidden),
               updated_at = $6
           WHERE id = $7"#,
    )
    .bind(&title)
    .bind(sort_name(&title))
    .bind(&release_type)
    .bind(year)
    .bind(body.is_hidden)
    .bind(&now)
    .bind(release_id)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query(
        r#"UPDATE furumusic__track
           SET year = $1, updated_at = $2
           WHERE release_id = $3"#,
    )
    .bind(year)
    .bind(&now)
    .bind(release_id)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if let Some(artist_names) = artist_names {
        replace_release_artists(pool, release_id, &artist_names).await?;
    }

    let page = load_user_uploads_page(pool, user.id, 500).await?;
    Json(page).into_response()
}

async fn user_upload_tracks_bulk_update_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<UserUploadBulkTrackUpdateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let mut track_ids = body
        .track_ids
        .into_iter()
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    track_ids.sort_unstable();
    track_ids.dedup();
    if track_ids.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "no tracks selected"));
    }
    if track_ids.len() > 500 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "too many tracks selected",
        ));
    }

    let release_ids = uploaded_track_release_ids(pool, user.id, &track_ids).await?;
    if release_ids.is_empty() {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "uploaded tracks not found",
        ));
    }
    if release_ids.len() > track_ids.len() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "invalid track selection",
        ));
    }

    let artist_names = match body.artist_names {
        Some(names) => match clean_artist_names(names) {
            Ok(names) => Some(names),
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        },
        None => None,
    };
    let featured_artist_names = match body.featured_artist_names {
        Some(names) => match clean_optional_artist_names(names) {
            Ok(names) => Some(names),
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
        },
        None => None,
    };
    let release_title = match clean_optional_string(body.release_title.as_deref(), 255) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let release_type = body.release_type.as_deref().map(normalize_release_type);
    let release_year = match parse_optional_i32(
        body.release_year.as_deref(),
        None,
        0,
        3000,
        "invalid release year",
    ) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let now = now_iso_string();

    if body.is_hidden.is_some() {
        sqlx::query(
            r#"UPDATE furumusic__track
               SET is_hidden = $1, updated_at = $2
               WHERE id = ANY($3)"#,
        )
        .bind(body.is_hidden)
        .bind(&now)
        .bind(&track_ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    if let Some(artist_names) = artist_names {
        for track_id in &track_ids {
            replace_track_role_artists(pool, *track_id, "main", &artist_names, 0).await?;
        }
    }
    if let Some(featured_artist_names) = featured_artist_names {
        for track_id in &track_ids {
            replace_track_role_artists(pool, *track_id, "featuring", &featured_artist_names, 1)
                .await?;
        }
    }
    if release_title.is_some() || release_type.is_some() || body.release_year.is_some() {
        for release_id in &release_ids {
            if !user_owns_release_tracks(pool, user.id, *release_id).await? {
                return Ok(json_error(
                    StatusCode::FORBIDDEN,
                    "release contains tracks uploaded by another user",
                ));
            }
            sqlx::query(
                r#"UPDATE furumusic__release
                   SET title = COALESCE($1, title),
                       title_sort = COALESCE($2, title_sort),
                       release_type = COALESCE($3, release_type),
                       year = CASE WHEN $4 THEN $5 ELSE year END,
                       updated_at = $6
                   WHERE id = $7"#,
            )
            .bind(release_title.as_ref())
            .bind(release_title.as_ref().map(|title| sort_name(title)))
            .bind(release_type.as_ref())
            .bind(body.release_year.is_some())
            .bind(release_year)
            .bind(&now)
            .bind(*release_id)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
            if body.release_year.is_some() {
                sqlx::query(
                    r#"UPDATE furumusic__track
                       SET year = $1, updated_at = $2
                       WHERE release_id = $3"#,
                )
                .bind(release_year)
                .bind(&now)
                .bind(*release_id)
                .execute(pool)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
            }
        }
    }

    let page = load_user_uploads_page(pool, user.id, 500).await?;
    Json(page).into_response()
}

async fn user_upload_review_save_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<UserUploadReviewUpdateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let review_id = path.0.id;
    if !user_owns_review(pool, user.id, review_id).await? {
        return Ok(json_error(StatusCode::NOT_FOUND, "upload review not found"));
    }
    let normalized = match normalized_from_upload_review_body(&body) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let result_json = serde_json::to_string(&normalized)
        .map_err(|e| cot::Error::internal(format!("failed to serialize review fields: {e}")))?;
    save_user_upload_review_result(&db, review_id, result_json).await?;
    let mut reviews = load_user_upload_reviews(pool, user.id, Some(review_id), 1)
        .await?
        .0;
    let Some(review) = reviews.pop() else {
        return Ok(json_error(StatusCode::NOT_FOUND, "upload review not found"));
    };
    Json(review).into_response()
}

async fn user_upload_review_delete_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let review_id = path.0.id;
    let uploaded_by_pattern = format!(r#""uploaded_by_user_id"\s*:\s*{}([^0-9]|$)"#, user.id);
    let result = sqlx::query(
        r#"DELETE FROM furumusic__pending_review
           WHERE id = $1
             AND context_json IS NOT NULL
             AND context_json ~ $2
             AND status IN ('pending', 'failed')"#,
    )
    .bind(review_id)
    .bind(uploaded_by_pattern)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if result.rows_affected() == 0 {
        return Ok(json_error(StatusCode::NOT_FOUND, "upload review not found"));
    }
    let page = load_user_uploads_page(pool, user.id, 500).await?;
    Json(page).into_response()
}

async fn user_upload_review_approve_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<UserUploadReviewUpdateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let review_id = path.0.id;
    if !user_owns_review(pool, user.id, review_id).await? {
        return Ok(json_error(StatusCode::NOT_FOUND, "upload review not found"));
    }
    let normalized = match normalized_from_upload_review_body(&body) {
        Ok(value) => value,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
    };
    let result_json = serde_json::to_string(&normalized)
        .map_err(|e| cot::Error::internal(format!("failed to serialize review fields: {e}")))?;
    let mut review = crate::scheduler::PendingReview::get_by_id(&db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("upload review not found"))?;
    let status = review.status.as_str();
    if status == "processing" || status == "approved" || status == "auto_approved" {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "review cannot be approved from this status",
        ));
    }
    review
        .set_result_json(&db, result_json)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;

    let context: serde_json::Value =
        serde_json::from_str(review.context_json_str()).unwrap_or_default();
    let (live_config, _) = AppConfig::load_with_db(&db).await;
    let input_path = crate::media_paths::resolve_path_from_root(
        &live_config.agent_inbox_dir,
        review.input_path_str(),
    );
    let input_path = input_path.to_string_lossy().to_string();
    match crate::jobs::inbox_process::finalize_approved(
        &db,
        pool,
        &live_config,
        &input_path,
        &normalized,
        &context,
        &live_config.agent_storage_dir,
        None,
    )
    .await
    {
        Ok(()) => {
            let _ = review.set_approved(&db).await;
            let page = load_user_uploads_page(pool, user.id, 500).await?;
            Json(page).into_response()
        }
        Err(err) => {
            let message = err.to_string();
            let _ = review.set_failed(&db, &message).await;
            Ok(json_error(StatusCode::BAD_REQUEST, &message))
        }
    }
}

async fn load_user_uploads_page(
    pool: &sqlx::PgPool,
    user_id: i64,
    limit: i64,
) -> cot::Result<UserUploadsPage> {
    let tracks = load_user_upload_tracks(pool, user_id, None, limit).await?;
    let releases = group_user_upload_releases(pool, &tracks).await?;
    let (pending, pending_total) = load_user_upload_reviews(pool, user_id, None, 100).await?;
    let (queued, queued_total) = load_user_upload_queue(pool, user_id).await?;
    Ok(UserUploadsPage {
        tracks,
        releases,
        pending,
        queued,
        pending_total,
        queued_total,
    })
}

async fn group_user_upload_releases(
    pool: &sqlx::PgPool,
    tracks: &[UserUploadTrack],
) -> cot::Result<Vec<UserUploadRelease>> {
    let mut release_ids = tracks
        .iter()
        .map(|item| item.track.release_id)
        .collect::<Vec<_>>();
    release_ids.sort_unstable();
    release_ids.dedup();

    let release_artists = if release_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, ReleaseArtistRefRow>(
            r#"SELECT ra.release_id,
                      a.id AS artist_id,
                      a.name::text AS artist_name
               FROM furumusic__release_artist ra
               JOIN furumusic__artist a ON a.id = ra.artist_id
               WHERE ra.release_id = ANY($1)
               ORDER BY ra.release_id, ra.position"#,
        )
        .bind(&release_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    };
    let mut artists_by_release: HashMap<i64, Vec<ArtistRef>> = HashMap::new();
    for row in release_artists {
        artists_by_release
            .entry(row.release_id)
            .or_default()
            .push(ArtistRef {
                id: row.artist_id,
                name: row.artist_name,
            });
    }

    let mut grouped: Vec<UserUploadRelease> = Vec::new();
    for track in tracks {
        let release_id = track.track.release_id;
        if let Some(release) = grouped.iter_mut().find(|release| release.id == release_id) {
            release.tracks.push(track.clone());
            continue;
        }
        grouped.push(UserUploadRelease {
            id: release_id,
            title: track.track.release_title.clone(),
            release_type: track.release_type.clone(),
            year: track.track.release_year,
            is_hidden: track.release_is_hidden,
            artists: artists_by_release.remove(&release_id).unwrap_or_default(),
            tracks: vec![track.clone()],
        });
    }
    grouped.sort_by(|a, b| {
        b.tracks
            .first()
            .map(|track| track.uploaded_at.as_str())
            .cmp(&a.tracks.first().map(|track| track.uploaded_at.as_str()))
    });
    Ok(grouped)
}

async fn load_user_upload_tracks(
    pool: &sqlx::PgPool,
    user_id: i64,
    track_id: Option<i64>,
    limit: i64,
) -> cot::Result<Vec<UserUploadTrack>> {
    let rows = sqlx::query_as::<_, UploadedTrackRow>(
        r#"SELECT t.id,
                  t.title::text AS title,
                  t.track_number,
                  t.disc_number,
                  t.duration_seconds,
                  t.cover_file_id,
                  r.cover_file_id AS release_cover_file_id,
                  r.id AS release_id,
                  r.title::text AS release_title,
                  r.release_type::text AS release_type,
                  r.year AS release_year,
                  r.is_hidden AS release_is_hidden,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at,
                  mf.id AS media_file_id,
                  t.is_hidden,
                  t.year,
                  mf.created_at::text AS uploaded_at
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE mf.uploaded_by_user_id = $1
             AND ($2::bigint IS NULL OR t.id = $2)
           ORDER BY mf.created_at DESC, t.id DESC
           LIMIT $3"#,
    )
    .bind(user_id)
    .bind(track_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let track_ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    let track_artists = if track_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, TrackArtistRow>(
            r#"SELECT ta.track_id, ta.artist_id, a.name::text as artist_name, ta.role::text as role
               FROM furumusic__track_artist ta
               JOIN furumusic__artist a ON a.id = ta.artist_id
               WHERE ta.track_id = ANY($1)
               ORDER BY ta.track_id, ta.position"#,
        )
        .bind(&track_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    };
    let mut main_artists: HashMap<i64, Vec<ArtistRef>> = HashMap::new();
    let mut featured_artists: HashMap<i64, Vec<ArtistRef>> = HashMap::new();
    for ta in track_artists {
        let artist = ArtistRef {
            id: ta.artist_id,
            name: ta.artist_name,
        };
        if ta.role == "featuring" {
            featured_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist);
        } else {
            main_artists.entry(ta.track_id).or_default().push(artist);
        }
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let track_id = row.id;
            UserUploadTrack {
                track: TrackItem {
                    id: row.id,
                    content_id: None,
                    title: row.title,
                    track_number: row.track_number,
                    disc_number: row.disc_number,
                    duration_seconds: row.duration_seconds,
                    artists: main_artists.remove(&track_id).unwrap_or_default(),
                    featured_artists: featured_artists.remove(&track_id).unwrap_or_default(),
                    release_id: row.release_id,
                    release_title: row.release_title,
                    release_year: row.release_year,
                    cover_url: track_cover_variant_url(
                        row.cover_file_id,
                        row.release_cover_file_id,
                        "medium",
                    ),
                    stream_url: format!("/api/player/stream/{track_id}"),
                    uploader_name: row.uploader_name,
                    audio_format: row.audio_format,
                    audio_bitrate: row.audio_bitrate,
                    audio_sample_rate: row.audio_sample_rate,
                    audio_bit_depth: row.audio_bit_depth,
                    file_size_bytes: row.file_size_bytes,
                    lastfm_listeners: row.lastfm_listeners,
                    lastfm_playcount: row.lastfm_playcount,
                    lastfm_rating: row.lastfm_rating,
                    lastfm_updated_at: row.lastfm_updated_at,
                },
                media_file_id: row.media_file_id,
                is_hidden: row.is_hidden,
                release_is_hidden: row.release_is_hidden,
                release_type: row.release_type,
                year: row.year,
                uploaded_at: row.uploaded_at,
            }
        })
        .collect())
}

async fn load_user_upload_reviews(
    pool: &sqlx::PgPool,
    user_id: i64,
    review_id: Option<i64>,
    limit: i64,
) -> cot::Result<(Vec<UserUploadReviewItem>, i64)> {
    let uploaded_by_pattern = format!(r#""uploaded_by_user_id"\s*:\s*{}([^0-9]|$)"#, user_id);
    let total = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*)
           FROM furumusic__pending_review
           WHERE context_json IS NOT NULL
             AND context_json ~ $1
             AND ($2::bigint IS NULL OR id = $2)
             AND status IN ('pending', 'failed')"#,
    )
    .bind(&uploaded_by_pattern)
    .bind(review_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let rows = sqlx::query_as::<_, UserUploadReviewRow>(
        r#"SELECT id,
                  status::text AS status,
                  input_path,
                  result_json,
                  context_json,
                  created_at::text AS created_at,
                  updated_at::text AS updated_at,
                  error_message
           FROM furumusic__pending_review
           WHERE context_json IS NOT NULL
             AND context_json ~ $1
             AND ($2::bigint IS NULL OR id = $2)
             AND status IN ('pending', 'failed')
           ORDER BY CASE status WHEN 'failed' THEN 0 ELSE 1 END, updated_at DESC
           LIMIT $3"#,
    )
    .bind(uploaded_by_pattern)
    .bind(review_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let items = rows
        .into_iter()
        .map(|row| UserUploadReviewItem {
            id: row.id,
            status: row.status,
            filename: input_path_filename(row.input_path.as_deref()),
            created_at: row.created_at,
            updated_at: row.updated_at,
            error_message: row.error_message,
            fields: review_fields_from_json(
                row.result_json.as_deref(),
                row.context_json.as_deref(),
            ),
        })
        .collect();
    Ok((items, total))
}

async fn load_user_upload_queue(
    pool: &sqlx::PgPool,
    user_id: i64,
) -> cot::Result<(Vec<UserUploadQueueItem>, i64)> {
    let uploaded_by_pattern = format!(r#""uploaded_by_user_id"\s*:\s*{}([^0-9]|$)"#, user_id);
    let total = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*)
           FROM furumusic__pending_review
           WHERE context_json IS NOT NULL
             AND context_json ~ $1
             AND status IN ('queued', 'processing')"#,
    )
    .bind(&uploaded_by_pattern)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let rows = sqlx::query_as::<_, UserUploadQueueRow>(
        r#"SELECT id,
                  status::text AS status,
                  input_path,
                  created_at::text AS created_at,
                  updated_at::text AS updated_at,
                  error_message
           FROM furumusic__pending_review
           WHERE context_json IS NOT NULL
             AND context_json ~ $1
             AND status IN ('queued', 'processing')
           ORDER BY
             CASE status WHEN 'processing' THEN 0 ELSE 1 END,
             created_at DESC
           LIMIT 100"#,
    )
    .bind(uploaded_by_pattern)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let items = rows
        .into_iter()
        .map(|row| UserUploadQueueItem {
            id: row.id,
            status: row.status,
            filename: input_path_filename(row.input_path.as_deref()),
            created_at: row.created_at,
            updated_at: row.updated_at,
            error_message: row.error_message,
        })
        .collect();
    Ok((items, total))
}

async fn user_owns_release_tracks(
    pool: &sqlx::PgPool,
    user_id: i64,
    release_id: i64,
) -> cot::Result<bool> {
    let other_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM furumusic__track t
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.release_id = $1
             AND COALESCE(mf.uploaded_by_user_id, -1) <> $2"#,
    )
    .bind(release_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(other_count == 0)
}

async fn user_owns_review(pool: &sqlx::PgPool, user_id: i64, review_id: i64) -> cot::Result<bool> {
    let uploaded_by_pattern = format!(r#""uploaded_by_user_id"\s*:\s*{}([^0-9]|$)"#, user_id);
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM furumusic__pending_review
           WHERE id = $1
             AND context_json IS NOT NULL
             AND context_json ~ $2"#,
    )
    .bind(review_id)
    .bind(uploaded_by_pattern)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(count > 0)
}

async fn uploaded_track_release_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    track_ids: &[i64],
) -> cot::Result<Vec<i64>> {
    let rows = sqlx::query_scalar::<_, i64>(
        r#"SELECT DISTINCT t.release_id
           FROM furumusic__track t
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.id = ANY($1)
             AND mf.uploaded_by_user_id = $2"#,
    )
    .bind(track_ids)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let owned_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM furumusic__track t
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.id = ANY($1)
             AND mf.uploaded_by_user_id = $2"#,
    )
    .bind(track_ids)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if owned_count != track_ids.len() as i64 {
        return Ok(Vec::new());
    }
    Ok(rows)
}

async fn save_user_upload_review_result(
    db: &Database,
    review_id: i64,
    result_json: String,
) -> cot::Result<()> {
    let mut review = crate::scheduler::PendingReview::get_by_id(db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("upload review not found"))?;
    review
        .set_result_json(db, result_json)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
    Ok(())
}

async fn replace_release_artists(
    pool: &sqlx::PgPool,
    release_id: i64,
    names: &[String],
) -> cot::Result<()> {
    sqlx::query("DELETE FROM furumusic__release_artist WHERE release_id = $1")
        .bind(release_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    for (position, name) in names.iter().enumerate() {
        let artist_id = find_or_create_player_artist(pool, name).await?;
        sqlx::query(
            r#"INSERT INTO furumusic__release_artist (release_id, artist_id, position)
               VALUES ($1, $2, $3)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(release_id)
        .bind(artist_id)
        .bind(position as i32)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    Ok(())
}

async fn replace_track_role_artists(
    pool: &sqlx::PgPool,
    track_id: i64,
    role: &str,
    names: &[String],
    position_offset: i32,
) -> cot::Result<()> {
    sqlx::query("DELETE FROM furumusic__track_artist WHERE track_id = $1 AND role = $2")
        .bind(track_id)
        .bind(role)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    for (index, name) in names.iter().enumerate() {
        let artist_id = find_or_create_player_artist(pool, name).await?;
        sqlx::query(
            r#"INSERT INTO furumusic__track_artist (track_id, artist_id, role, position)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(track_id)
        .bind(artist_id)
        .bind(role)
        .bind(position_offset + index as i32)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    Ok(())
}

async fn find_or_create_player_artist(pool: &sqlx::PgPool, name: &str) -> cot::Result<i64> {
    let name = name.trim();
    let sort = sort_name(name);
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM furumusic__artist WHERE name_sort = $1 ORDER BY id LIMIT 1",
    )
    .bind(&sort)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?
    {
        return Ok(id);
    }

    let now = now_iso_string();
    sqlx::query_scalar(
        r#"INSERT INTO furumusic__artist (name, name_sort, image_file_id, is_hidden, model_name, created_at, updated_at)
           VALUES ($1, $2, NULL, false, NULL, $3, $3)
           RETURNING id"#,
    )
    .bind(name)
    .bind(sort)
    .bind(now)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

fn now_iso_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sort_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn normalize_release_type(value: &str) -> String {
    let value = value.trim().to_lowercase();
    if crate::music::RELEASE_TYPES
        .iter()
        .any(|(code, _, _)| *code == value)
    {
        value
    } else {
        "album".to_string()
    }
}

fn clean_required_string(
    raw: Option<&str>,
    fallback: &str,
    max_len: usize,
) -> Result<String, &'static str> {
    let value = raw.unwrap_or(fallback).trim();
    if value.is_empty() {
        return Err("value cannot be empty");
    }
    Ok(value.chars().take(max_len).collect())
}

fn parse_optional_i32(
    raw: Option<&str>,
    fallback: Option<i32>,
    min: i32,
    max: i32,
    error: &'static str,
) -> Result<Option<i32>, &'static str> {
    let Some(raw) = raw else {
        return Ok(fallback);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let value = raw.parse::<i32>().map_err(|_| error)?;
    if value < min || value > max {
        return Err(error);
    }
    Ok(Some(value))
}

fn clean_artist_names(raw_names: Vec<String>) -> Result<Vec<String>, &'static str> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for raw_name in raw_names {
        let name = raw_name.trim();
        if name.is_empty() {
            continue;
        }
        let name: String = name.chars().take(255).collect();
        if seen.insert(sort_name(&name)) {
            names.push(name);
        }
        if names.len() >= 12 {
            break;
        }
    }
    if names.is_empty() {
        return Err("at least one artist is required");
    }
    Ok(names)
}

fn clean_optional_artist_names(raw_names: Vec<String>) -> Result<Vec<String>, &'static str> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for raw_name in raw_names {
        let name = raw_name.trim();
        if name.is_empty() {
            continue;
        }
        let name: String = name.chars().take(255).collect();
        if seen.insert(sort_name(&name)) {
            names.push(name);
        }
        if names.len() >= 12 {
            break;
        }
    }
    Ok(names)
}

fn clean_optional_string(
    raw: Option<&str>,
    max_len: usize,
) -> Result<Option<String>, &'static str> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value.chars().take(max_len).collect()))
}

fn review_fields_from_json(
    result_json: Option<&str>,
    context_json: Option<&str>,
) -> UserUploadReviewFields {
    let normalized = result_json
        .and_then(|value| serde_json::from_str::<crate::agent::dto::NormalizedFields>(value).ok())
        .unwrap_or_default();
    let context = context_json
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .unwrap_or_default();
    let ctx_str = |key: &str| {
        context
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let ctx_i32 = |key: &str| {
        context
            .get(key)
            .and_then(|value| value.as_i64())
            .map(|value| value.to_string())
            .unwrap_or_default()
    };
    UserUploadReviewFields {
        title: normalized
            .title
            .unwrap_or_else(|| first_non_empty([ctx_str("raw_title"), ctx_str("path_title")])),
        artist: normalized
            .artist
            .unwrap_or_else(|| first_non_empty([ctx_str("raw_artist"), ctx_str("path_artist")])),
        album: normalized
            .album
            .unwrap_or_else(|| first_non_empty([ctx_str("raw_album"), ctx_str("path_album")])),
        year: normalized
            .year
            .map(|value| value.to_string())
            .unwrap_or_else(|| first_non_empty([ctx_i32("raw_year"), ctx_i32("path_year")])),
        track_number: normalized
            .track_number
            .map(|value| value.to_string())
            .unwrap_or_else(|| {
                first_non_empty([ctx_i32("raw_track_number"), ctx_i32("path_track_number")])
            }),
        genre: normalized.genre.unwrap_or_else(|| ctx_str("raw_genre")),
        featured_artists: normalized.featured_artists,
        release_type: normalized
            .release_type
            .unwrap_or_else(|| "album".to_owned()),
        notes: normalized.notes.unwrap_or_default(),
    }
}

fn first_non_empty<const N: usize>(values: [String; N]) -> String {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

fn normalized_from_upload_review_body(
    body: &UserUploadReviewUpdateRequest,
) -> Result<crate::agent::dto::NormalizedFields, &'static str> {
    let title = clean_required_string(body.title.as_deref(), "", 255)?;
    let artist = clean_required_string(body.artist.as_deref(), "", 255)?;
    let album = clean_required_string(body.album.as_deref(), "", 255)?;
    let year = parse_optional_i32(body.year.as_deref(), None, 0, 3000, "invalid release year")?;
    let track_number = parse_optional_i32(
        body.track_number.as_deref(),
        None,
        1,
        999,
        "invalid track number",
    )?;
    let featured_artists =
        clean_optional_artist_names(body.featured_artists.clone().unwrap_or_default())?;
    Ok(crate::agent::dto::NormalizedFields {
        title: Some(title),
        artist: Some(artist),
        album: Some(album),
        year,
        track_number,
        genre: clean_optional_string(body.genre.as_deref(), 255)?,
        featured_artists,
        release_type: Some(
            body.release_type
                .as_deref()
                .map(normalize_release_type)
                .unwrap_or_else(|| "album".to_owned()),
        ),
        confidence: Some(1.0),
        notes: clean_optional_string(body.notes.as_deref(), 2000)?,
    })
}

fn input_path_filename(path: Option<&str>) -> String {
    path.and_then(|path| path.rsplit(['/', '\\']).next())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("queued track")
        .to_string()
}

// ---------------------------------------------------------------------------
// GET /api/player/artists?page=N&limit=N
// ---------------------------------------------------------------------------

async fn artists_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    query: cot::request::extractors::UrlQuery<PaginationQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let page = query.0.page.unwrap_or(1).max(1);
    let per_page = query.0.limit.unwrap_or(60).clamp(1, 200);
    let offset = (page - 1) as i64 * per_page as i64;
    let mine = query.0.mine.unwrap_or(false);

    if mine {
        let total_row = sqlx::query_as::<_, CountRow>(
            r#"SELECT COUNT(DISTINCT a.id) AS count
               FROM furumusic__artist a
               JOIN furumusic__track_artist ta ON ta.artist_id = a.id
               JOIN furumusic__track t ON t.id = ta.track_id AND t.is_hidden = false
               JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
               JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
              WHERE a.is_hidden = false
                AND mf.uploaded_by_user_id = $1"#,
        )
        .bind(user.id)
        .fetch_one(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

        let rows = sqlx::query_as::<_, ArtistRow>(
            r#"SELECT a.id, a.name::text as name, a.image_file_id,
                      COUNT(DISTINCT r.id) FILTER (WHERE primary_release.artist_id IS NOT NULL) AS release_count,
                      COUNT(DISTINCT t.id) AS track_count
               FROM furumusic__artist a
               JOIN furumusic__track_artist ta ON ta.artist_id = a.id
               JOIN furumusic__track t ON t.id = ta.track_id AND t.is_hidden = false
               JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
               JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
               LEFT JOIN furumusic__release_artist primary_release
                 ON primary_release.release_id = r.id
                AND primary_release.artist_id = a.id
                AND primary_release.position = 0
              WHERE a.is_hidden = false
                AND mf.uploaded_by_user_id = $1
              GROUP BY a.id, a.name, a.name_sort, a.image_file_id
              ORDER BY (COUNT(DISTINCT r.id) FILTER (WHERE primary_release.artist_id IS NOT NULL) > 0) DESC,
                       release_count DESC,
                       track_count DESC,
                       a.name_sort
              LIMIT $2 OFFSET $3"#,
        )
        .bind(user.id)
        .bind(per_page as i64)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

        let items: Vec<ArtistCard> = rows
            .into_iter()
            .map(|r| ArtistCard {
                id: r.id,
                name: r.name,
                image_url: cover_variant_url(r.image_file_id, "medium"),
                release_count: r.release_count,
                track_count: r.track_count,
            })
            .collect();
        let has_more = offset + (items.len() as i64) < total_row.count;

        return Json(Paginated {
            items,
            total: total_row.count,
            page,
            per_page,
            has_more,
        })
        .into_response();
    }

    let total_row = sqlx::query_as::<_, CountRow>(
        r#"SELECT COUNT(*) AS count
           FROM furumusic__artist a
           WHERE a.is_hidden = false"#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let rows = sqlx::query_as::<_, ArtistRow>(
        r#"SELECT a.id, a.name::text as name, a.image_file_id,
                  COALESCE(s.release_count, 0) AS release_count,
                  COALESCE(s.track_count, 0) AS track_count
           FROM furumusic__artist a
           LEFT JOIN (
               SELECT appearance.artist_id,
                      COUNT(DISTINCT appearance.release_id) FILTER (WHERE appearance.is_primary_release_artist) AS release_count,
                      COUNT(DISTINCT appearance.track_id) AS track_count
                 FROM (
                        SELECT ta.artist_id,
                               t.id AS track_id,
                               r.id AS release_id,
                               primary_release.artist_id IS NOT NULL AS is_primary_release_artist
                          FROM furumusic__track_artist ta
                          JOIN furumusic__track t ON t.id = ta.track_id AND t.is_hidden = false
                          JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
                          LEFT JOIN furumusic__release_artist primary_release
                            ON primary_release.release_id = r.id
                           AND primary_release.artist_id = ta.artist_id
                           AND primary_release.position = 0
                      ) appearance
                GROUP BY appearance.artist_id
           ) s ON s.artist_id = a.id
           WHERE a.is_hidden = false
           ORDER BY (COALESCE(s.release_count, 0) > 0) DESC,
                    COALESCE(s.release_count, 0) DESC,
                    COALESCE(s.track_count, 0) DESC,
                    a.name_sort
           LIMIT $1 OFFSET $2"#,
    )
    .bind(per_page as i64)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let items: Vec<ArtistCard> = rows
        .into_iter()
        .map(|r| ArtistCard {
            id: r.id,
            name: r.name,
            image_url: cover_variant_url(r.image_file_id, "medium"),
            release_count: r.release_count,
            track_count: r.track_count,
        })
        .collect();
    let has_more = offset + (items.len() as i64) < total_row.count;

    Json(Paginated {
        items,
        total: total_row.count,
        page,
        per_page,
        has_more,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/artists/{id}
// ---------------------------------------------------------------------------

async fn artist_detail_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let artist_id = path.0.id;

    let artist = sqlx::query_as::<_, ArtistBriefRow>(
        "SELECT id, name::text as name FROM furumusic__artist WHERE id = $1 AND is_hidden = false",
    )
    .bind(artist_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(artist) = artist else {
        return Ok(json_error(StatusCode::NOT_FOUND, "artist not found"));
    };

    let image_file_id: Option<i64> =
        sqlx::query_scalar("SELECT image_file_id FROM furumusic__artist WHERE id = $1")
            .bind(artist_id)
            .fetch_one(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;

    let releases = sqlx::query_as::<_, ReleaseRow>(
        r#"SELECT r.id, r.title::text as title, r.release_type::text as release_type,
                  r.year, r.cover_file_id,
                  COALESCE((SELECT COUNT(*) FROM furumusic__track t WHERE t.release_id = r.id AND t.is_hidden = false), 0) as track_count
           FROM furumusic__release r
           JOIN furumusic__release_artist ra ON ra.release_id = r.id
           WHERE ra.artist_id = $1 AND r.is_hidden = false
           ORDER BY r.year DESC NULLS LAST, r.title_sort"#,
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let release_ids: Vec<i64> = releases.iter().map(|r| r.id).collect();
    let mut release_uploaders = load_release_uploaders(pool, &release_ids)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let release_cards: Vec<ReleaseCard> = releases
        .into_iter()
        .map(|r| ReleaseCard {
            id: r.id,
            title: r.title,
            release_type: r.release_type,
            year: r.year,
            cover_url: cover_variant_url(r.cover_file_id, "medium"),
            track_count: r.track_count,
            uploaders: release_uploaders.remove(&r.id).unwrap_or_default(),
        })
        .collect();

    let total_track_count = release_cards.iter().map(|r| r.track_count).sum();
    let total_play_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM furumusic__play_history ph
           JOIN furumusic__track t ON t.id = ph.track_id
           JOIN furumusic__release_artist ra ON ra.release_id = t.release_id
           JOIN furumusic__release r ON r.id = t.release_id
           WHERE ra.artist_id = $1 AND t.is_hidden = false AND r.is_hidden = false"#,
    )
    .bind(artist_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let featured_rows = sqlx::query_as::<_, AppearanceTrackRow>(
        r#"SELECT DISTINCT t.id,
                  t.title::text AS title,
                  r.id AS release_id,
                  r.title::text AS release_title,
                  r.year AS release_year,
                  t.duration_seconds,
                  t.cover_file_id,
                  r.cover_file_id AS release_cover_file_id,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
           FROM furumusic__track_artist ta
           JOIN furumusic__track t ON t.id = ta.track_id
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE ta.artist_id = $1
             AND ta.role = 'featuring'
             AND t.is_hidden = false
             AND r.is_hidden = false
           ORDER BY r.title::text, t.title::text"#,
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let featured_track_ids: Vec<i64> = featured_rows.iter().map(|t| t.id).collect();
    let featured_track_artists = if featured_track_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, TrackArtistRow>(
            r#"SELECT ta.track_id, ta.artist_id, a.name::text as artist_name, ta.role::text as role
               FROM furumusic__track_artist ta
               JOIN furumusic__artist a ON a.id = ta.artist_id
               WHERE ta.track_id = ANY($1)
               ORDER BY ta.track_id, ta.position"#,
        )
        .bind(&featured_track_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    };

    let mut featured_main_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();
    let mut featured_feat_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();

    for ta in &featured_track_artists {
        let artist_ref = ArtistRef {
            id: ta.artist_id,
            name: ta.artist_name.clone(),
        };
        if ta.role == "featuring" {
            featured_feat_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        } else {
            featured_main_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        }
    }

    let featured_tracks: Vec<ArtistAppearanceTrack> = featured_rows
        .into_iter()
        .map(|t| {
            let tid = t.id;
            ArtistAppearanceTrack {
                id: t.id,
                title: t.title,
                release_id: t.release_id,
                release_title: t.release_title,
                release_year: t.release_year,
                duration_seconds: t.duration_seconds,
                artists: featured_main_artists.remove(&tid).unwrap_or_default(),
                featured_artists: featured_feat_artists.remove(&tid).unwrap_or_default(),
                cover_url: track_cover_variant_url(
                    t.cover_file_id,
                    t.release_cover_file_id,
                    "medium",
                ),
                stream_url: format!("/api/player/stream/{tid}"),
                uploader_name: t.uploader_name,
                audio_format: t.audio_format,
                audio_bitrate: t.audio_bitrate,
                audio_sample_rate: t.audio_sample_rate,
                audio_bit_depth: t.audio_bit_depth,
                file_size_bytes: t.file_size_bytes,
                lastfm_listeners: t.lastfm_listeners,
                lastfm_playcount: t.lastfm_playcount,
                lastfm_rating: t.lastfm_rating,
                lastfm_updated_at: t.lastfm_updated_at,
            }
        })
        .collect();

    let top_tracks = sqlx::query_as::<_, PlaylistTrackRow>(
        r#"SELECT * FROM (
               SELECT DISTINCT ON (lower(t.title::text))
                      NULL::bigint AS playlist_track_id,
                      t.id, t.title::text as title, t.track_number, t.disc_number,
                      t.duration_seconds, t.cover_file_id,
                      r.cover_file_id as release_cover_file_id,
                      r.id as release_id,
                      r.title::text as release_title,
                      r.year as release_year,
                      COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                      mf.audio_format,
                      mf.audio_bitrate,
                      mf.audio_sample_rate,
                      mf.audio_bit_depth,
                      mf.file_size_bytes,
                      t.lastfm_listeners,
                      t.lastfm_playcount,
                      t.lastfm_rating,
                      t.lastfm_updated_at
               FROM furumusic__track t
               JOIN furumusic__release r ON r.id = t.release_id
               LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
               WHERE t.is_hidden = false
                 AND r.is_hidden = false
                 AND EXISTS (
                     SELECT 1
                       FROM furumusic__track_artist ta
                      WHERE ta.track_id = t.id
                        AND ta.artist_id = $1
                        AND ta.role <> 'featuring'
                 )
               ORDER BY lower(t.title::text), COALESCE(t.lastfm_rating, 0) DESC
           ) deduped
           ORDER BY COALESCE(lastfm_rating, 0) DESC,
                    COALESCE(lastfm_playcount, 0) DESC,
                    COALESCE(lastfm_listeners, 0) DESC,
                    release_year DESC NULLS LAST,
                    track_number NULLS LAST,
                    id
           LIMIT 50"#,
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let top_tracks = build_track_items(top_tracks, pool).await?;

    Json(ArtistDetail {
        id: artist.id,
        name: artist.name,
        image_url: cover_variant_url(image_file_id, "large"),
        total_track_count,
        total_play_count,
        top_tracks,
        releases: release_cards,
        featured_tracks,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/releases/{id}
// ---------------------------------------------------------------------------

async fn release_detail_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let release_id = path.0.id;

    let release = sqlx::query_as::<_, ReleaseInfoRow>(
        r#"SELECT id, title::text as title, release_type::text as release_type, year, cover_file_id
           FROM furumusic__release WHERE id = $1 AND is_hidden = false"#,
    )
    .bind(release_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(release) = release else {
        return Ok(json_error(StatusCode::NOT_FOUND, "release not found"));
    };

    // Release artists
    let release_artists = sqlx::query_as::<_, ArtistBriefRow>(
        r#"SELECT a.id, a.name::text as name
           FROM furumusic__artist a
           JOIN furumusic__release_artist ra ON ra.artist_id = a.id
           WHERE ra.release_id = $1
           ORDER BY ra.position"#,
    )
    .bind(release_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let tracks = sqlx::query_as::<_, PlaylistTrackRow>(
        r#"SELECT NULL::bigint AS playlist_track_id,
                  t.id, t.title::text as title, t.track_number, t.disc_number,
                  t.duration_seconds, t.cover_file_id,
                  r.cover_file_id as release_cover_file_id,
                  r.id as release_id,
                  r.title::text as release_title,
                  r.year as release_year,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.release_id = $1 AND t.is_hidden = false
           ORDER BY t.disc_number NULLS FIRST, t.track_number NULLS LAST"#,
    )
    .bind(release_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let track_items = build_track_items(tracks, pool).await?;
    let uploaders = load_release_uploaders(pool, &[release.id])
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
        .remove(&release.id)
        .unwrap_or_default();

    Json(ReleaseDetail {
        id: release.id,
        title: release.title,
        release_type: release.release_type,
        year: release.year,
        cover_url: cover_variant_url(release.cover_file_id, "large"),
        artists: release_artists
            .into_iter()
            .map(|a| ArtistRef {
                id: a.id,
                name: a.name,
            })
            .collect(),
        tracks: track_items,
        uploaders,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/playlists
// ---------------------------------------------------------------------------

async fn playlists_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    // Count liked tracks for the virtual Likes playlist
    let likes_count: (i64,) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM furumusic__user_liked_track WHERE user_id = $1)
          + (SELECT COUNT(*) FROM furumusic__fed_state_like
              WHERE user_id = $1 AND liked = true AND local_track_id IS NULL)",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let mut cards = vec![PlaylistCard {
        id: -1,
        title: "Likes".to_string(),
        track_count: likes_count.0,
        is_own: true,
        owner_name: None,
        is_public: false,
        is_saved: false,
        kind: "likes".to_string(),
    }];

    let rows = sqlx::query_as::<_, PlaylistRow>(
        r#"SELECT p.id, p.title::text as title,
                  COALESCE((SELECT COUNT(*) FROM furumusic__playlist_track pt WHERE pt.playlist_id = p.id), 0) as track_count,
                  (p.owner_id = $1) as is_own,
                  COALESCE(NULLIF(u.display_name, ''), u.username)::text as owner_name,
                  p.is_public,
                  EXISTS (
                      SELECT 1 FROM furumusic__saved_playlist sp
                      WHERE sp.user_id = $1 AND sp.playlist_id = p.id
                  ) as is_saved
           FROM furumusic__playlist p
           JOIN furumusic__user u ON u.id = p.owner_id
           WHERE p.owner_id = $1
              OR EXISTS (
                  SELECT 1 FROM furumusic__saved_playlist sp
                  WHERE sp.user_id = $1 AND sp.playlist_id = p.id
              )
              OR p.is_public = true
           ORDER BY
              CASE WHEN p.owner_id = $1 THEN 0 WHEN p.is_public THEN 2 ELSE 1 END,
              p.title"#,
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    for r in rows {
        let federated_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM furumusic__fed_state_playlist_item i
               JOIN furumusic__fed_state_playlist p
                 ON p.user_id = i.user_id AND p.playlist_id = i.playlist_id
              WHERE i.user_id = $1 AND p.local_playlist_id = $2
                AND i.present = true AND i.local_track_id IS NULL",
        )
        .bind(user.id)
        .bind(r.id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        cards.push(PlaylistCard {
            id: r.id,
            title: r.title,
            track_count: r.track_count + federated_count,
            is_own: r.is_own,
            owner_name: Some(r.owner_name),
            is_public: r.is_public,
            is_saved: r.is_saved,
            kind: "user".to_string(),
        });
    }

    Json(cards).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/playlists/{id}
// ---------------------------------------------------------------------------

async fn playlist_detail_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let playlist_id = path.0.id;

    // Virtual Likes playlist (id = -1)
    if playlist_id == -1 {
        return likes_playlist_handler(user.id, pool).await;
    }

    let info = sqlx::query_as::<_, PlaylistInfoRow>(
        r#"SELECT p.id, p.title::text as title, p.description, p.owner_id,
                  COALESCE(NULLIF(u.display_name, ''), u.username)::text as owner_name,
                  p.is_public,
                  EXISTS (
                      SELECT 1 FROM furumusic__saved_playlist sp
                      WHERE sp.user_id = $2 AND sp.playlist_id = p.id
                  ) as is_saved
           FROM furumusic__playlist p
           JOIN furumusic__user u ON u.id = p.owner_id
           WHERE p.id = $1"#,
    )
    .bind(playlist_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(info) = info else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };

    let tracks = sqlx::query_as::<_, PlaylistTrackRow>(
        r#"SELECT pt.id AS playlist_track_id,
                  t.id, t.title::text as title, t.track_number, t.disc_number,
                  t.duration_seconds, t.cover_file_id,
                  r.cover_file_id as release_cover_file_id,
                  r.id as release_id,
                  r.title::text as release_title,
                  r.year as release_year,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
           FROM furumusic__playlist_track pt
           JOIN furumusic__track t ON t.id = pt.track_id
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE pt.playlist_id = $1 AND t.is_hidden = false
           ORDER BY pt.position"#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let track_items = build_playlist_track_items(tracks, pool).await?;

    Json(PlaylistDetail {
        id: info.id,
        title: info.title,
        description: info.description,
        is_own: info.owner_id == user.id,
        owner_name: Some(info.owner_name),
        is_public: info.is_public,
        is_saved: info.is_saved,
        kind: "user".to_string(),
        tracks: track_items,
    })
    .into_response()
}

/// Shared helper: given PlaylistTrackRows, fetch artists and build TrackItems.
async fn build_track_items(
    tracks: Vec<PlaylistTrackRow>,
    pool: &sqlx::PgPool,
) -> cot::Result<Vec<TrackItem>> {
    let track_ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
    let mut content_ids: HashMap<i64, String> = if track_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query(
            "SELECT t.id AS track_id, c.content_id
               FROM furumusic__track t
               JOIN furumusic__media_file m ON m.id = t.audio_file_id
               JOIN furumusic__federation_content_id_cache c
                 ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
              WHERE t.id = ANY($1)",
        )
        .bind(&track_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
        .into_iter()
        .map(|row| (row.get("track_id"), row.get("content_id")))
        .collect()
    };

    let track_artists = if track_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, TrackArtistRow>(
            r#"SELECT ta.track_id, ta.artist_id, a.name::text as artist_name, ta.role::text as role
               FROM furumusic__track_artist ta
               JOIN furumusic__artist a ON a.id = ta.artist_id
               WHERE ta.track_id = ANY($1)
               ORDER BY ta.track_id, ta.position"#,
        )
        .bind(&track_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    };

    let mut track_main_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();
    let mut track_feat_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();

    for ta in &track_artists {
        let artist_ref = ArtistRef {
            id: ta.artist_id,
            name: ta.artist_name.clone(),
        };
        if ta.role == "featuring" {
            track_feat_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        } else {
            track_main_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        }
    }

    Ok(tracks
        .into_iter()
        .map(|t| {
            let tid = t.id;
            TrackItem {
                id: t.id,
                content_id: content_ids.remove(&tid),
                title: t.title,
                track_number: t.track_number,
                disc_number: t.disc_number,
                duration_seconds: t.duration_seconds,
                artists: track_main_artists.remove(&tid).unwrap_or_default(),
                featured_artists: track_feat_artists.remove(&tid).unwrap_or_default(),
                release_id: t.release_id,
                release_title: t.release_title,
                release_year: t.release_year,
                cover_url: track_cover_variant_url(
                    t.cover_file_id,
                    t.release_cover_file_id,
                    "medium",
                ),
                stream_url: format!("/api/player/stream/{tid}"),
                uploader_name: t.uploader_name,
                audio_format: t.audio_format,
                audio_bitrate: t.audio_bitrate,
                audio_sample_rate: t.audio_sample_rate,
                audio_bit_depth: t.audio_bit_depth,
                file_size_bytes: t.file_size_bytes,
                lastfm_listeners: t.lastfm_listeners,
                lastfm_playcount: t.lastfm_playcount,
                lastfm_rating: t.lastfm_rating,
                lastfm_updated_at: t.lastfm_updated_at,
            }
        })
        .collect())
}

async fn build_playlist_track_items(
    tracks: Vec<PlaylistTrackRow>,
    pool: &sqlx::PgPool,
) -> cot::Result<Vec<PlaylistTrackItem>> {
    let playlist_track_ids = tracks
        .iter()
        .map(|track| track.playlist_track_id)
        .collect::<Vec<_>>();
    let track_items = build_track_items(tracks, pool).await?;

    Ok(track_items
        .into_iter()
        .zip(playlist_track_ids)
        .map(|(track, playlist_track_id)| PlaylistTrackItem {
            playlist_track_id,
            sort_key: None,
            track,
        })
        .collect())
}

async fn load_track_items_by_ids(pool: &sqlx::PgPool, ids: &[i64]) -> cot::Result<Vec<TrackItem>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let tracks = sqlx::query_as::<_, PlaylistTrackRow>(
        r#"SELECT NULL::bigint AS playlist_track_id,
                  t.id, t.title::text as title, t.track_number, t.disc_number,
                  t.duration_seconds, t.cover_file_id,
                  r.cover_file_id as release_cover_file_id,
                  r.id as release_id,
                  r.title::text as release_title,
                  r.year as release_year,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.id = ANY($1) AND t.is_hidden = false AND r.is_hidden = false"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let track_map: HashMap<i64, TrackItem> = build_track_items(tracks, pool)
        .await?
        .into_iter()
        .map(|track| (track.id, track))
        .collect();

    Ok(ids
        .iter()
        .filter_map(|id| track_map.get(id).cloned())
        .collect())
}

// ---------------------------------------------------------------------------
// POST /api/player/share-playlist
// ---------------------------------------------------------------------------

async fn create_playlist_share_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<CreatePlaylistShareRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let ids: Vec<i64> = body
        .track_ids
        .into_iter()
        .filter(|id| *id > 0)
        .take(500)
        .collect();
    if ids.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "playlist is empty"));
    }

    let tracks = load_track_items_by_ids(pool, &ids).await?;
    if tracks.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "playlist has no visible tracks",
        ));
    }

    let visible_ids: Vec<i64> = tracks.iter().map(|track| track.id).collect();
    let track_ids_json =
        serde_json::to_string(&visible_ids).map_err(|e| cot::Error::internal(e.to_string()))?;
    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Shared queue")
        .chars()
        .take(160)
        .collect::<String>();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    sqlx::query(
        r#"INSERT INTO furumusic__playlist_share_link
           (token, creator_user_id, title, track_ids_json, created_at)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(&token)
    .bind(user.id)
    .bind(&title)
    .bind(&track_ids_json)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(ShareLinkResponse {
        token: token.clone(),
        url: format!("/share/playlist/{token}"),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/share-playlist/{id}
// ---------------------------------------------------------------------------

async fn playlist_share_detail_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathStringId>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let token = path.0.id.trim().to_string();
    let share: Option<(String, String)> = sqlx::query_as(
        r#"SELECT title::text, track_ids_json::text
           FROM furumusic__playlist_share_link
           WHERE token = $1"#,
    )
    .bind(&token)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some((title, track_ids_json)) = share else {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "shared playlist not found",
        ));
    };

    let ids: Vec<i64> = serde_json::from_str(&track_ids_json).unwrap_or_default();
    let tracks = load_track_items_by_ids(pool, &ids).await?;

    Json(PlaylistShareDetail {
        token,
        title,
        tracks,
    })
    .into_response()
}

/// Return the virtual "Likes" playlist for a given user.
async fn likes_playlist_handler(
    user_id: i64,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let tracks = sqlx::query_as::<_, PlaylistTrackRow>(
        r#"SELECT NULL::bigint AS playlist_track_id,
                  t.id, t.title::text as title, t.track_number, t.disc_number,
                  t.duration_seconds, t.cover_file_id,
                  r.cover_file_id as release_cover_file_id,
                  r.id as release_id,
                  r.title::text as release_title,
                  r.year as release_year,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate,
                  mf.audio_sample_rate,
                  mf.audio_bit_depth,
                  mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
           FROM furumusic__user_liked_track ult
           JOIN furumusic__track t ON t.id = ult.track_id
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE ult.user_id = $1 AND t.is_hidden = false
           ORDER BY ult.created_at DESC"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let order_rows = sqlx::query(
        "SELECT ult.track_id,
                COALESCE(
                    fsl.hlc_ms,
                    (EXTRACT(EPOCH FROM ult.created_at::timestamptz) * 1000)::bigint,
                    0
                ) AS sort_key
           FROM furumusic__user_liked_track ult
           JOIN furumusic__track t ON t.id = ult.track_id
           LEFT JOIN furumusic__media_file m ON m.id = t.audio_file_id
           LEFT JOIN furumusic__federation_content_id_cache c
                  ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
           LEFT JOIN furumusic__fed_state_like fsl
                  ON fsl.user_id = ult.user_id
                 AND fsl.content_id = c.content_id
                 AND fsl.liked = true
          WHERE ult.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let order_by_track = order_rows
        .into_iter()
        .map(|row| (row.get::<i64, _>("track_id"), row.get::<i64, _>("sort_key")))
        .collect::<HashMap<_, _>>();

    let mut track_items = build_track_items(tracks, pool)
        .await?
        .into_iter()
        .map(|track| PlaylistTrackItem {
            playlist_track_id: None,
            sort_key: order_by_track.get(&track.id).copied(),
            track,
        })
        .collect::<Vec<_>>();
    track_items.sort_by(|left, right| {
        right
            .sort_key
            .cmp(&left.sort_key)
            .then_with(|| left.track.content_id.cmp(&right.track.content_id))
            .then_with(|| left.track.title.cmp(&right.track.title))
            .then_with(|| left.track.id.cmp(&right.track.id))
    });

    Json(PlaylistDetail {
        id: -1,
        title: "Likes".to_string(),
        description: None,
        is_own: true,
        owner_name: None,
        is_public: false,
        is_saved: false,
        kind: "likes".to_string(),
        tracks: track_items,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/stream/{track_id}  — Range-aware audio streaming
// ---------------------------------------------------------------------------

async fn stream_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    request: &cot::http::Request<Body>,
    path: Path<PathTrackId>,
) -> cot::Result<cot::http::Response<Body>> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let track_id = path.0.track_id;

    // Look up track → audio_file_id → MediaFile
    let media = sqlx::query_as::<_, MediaFileRow>(
        r#"SELECT mf.file_path,
                  mf.mime_type::text as mime_type,
                  mf.file_size_bytes,
                  mf.sha256_hash::text AS sha256_hash,
                  mf.created_at::text AS created_at
           FROM furumusic__track t
           JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.id = $1"#,
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(media) = media else {
        return Ok(json_error(StatusCode::NOT_FOUND, "track not found"));
    };

    let full_path =
        crate::media_paths::resolve_media_file_path(&config.agent_storage_dir, &media.file_path);

    if !full_path.exists() {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "audio file not found on disk",
        ));
    }

    let file_size = media.file_size_bytes as u64;
    let etag = media_etag("audio", track_id, &media.sha256_hash, None);

    // Parse Range header
    let range_header = request.headers().get(RANGE).and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // Parse "bytes=START-END" or "bytes=START-"
        if let Some(range) = parse_range(range_str, file_size) {
            let (start, end) = range;
            let chunk_size = end - start + 1;

            let data = read_file_range(&full_path, start, chunk_size).await?;
            crate::metrics::record_stream_request(true, chunk_size);

            let response = cot::http::Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_TYPE, media.mime_type.as_str())
                .header(ACCEPT_RANGES, "bytes")
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{file_size}"))
                .header(CONTENT_LENGTH, chunk_size.to_string())
                .header("ETag", etag.as_str())
                .header("X-Furumi-Content-Sha256", media.sha256_hash.as_str())
                .header("X-Furumi-Content-Updated-At", media.created_at.as_str())
                .body(Body::fixed(data))
                .expect("valid response");

            return Ok(response);
        }
    }

    // No Range or invalid range: return full file
    let data = tokio::fs::read(&full_path)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    crate::metrics::record_stream_request(false, file_size);

    let response = cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, media.mime_type.as_str())
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, file_size.to_string())
        .header("ETag", etag.as_str())
        .header("X-Furumi-Content-Sha256", media.sha256_hash.as_str())
        .header("X-Furumi-Content-Updated-At", media.created_at.as_str())
        .body(Body::fixed(data))
        .expect("valid response");

    Ok(response)
}

async fn federation_cache_stream_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    request: cot::request::Request,
    Path(path): Path<PathStringId>,
) -> cot::Result<cot::http::Response<Body>> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some((full_path, mime)) = crate::federation::handle().prepared_cache_file(&path.id) else {
        return Ok(json_error(
            StatusCode::NOT_FOUND,
            "federated cache entry not found",
        ));
    };
    let file_size = tokio::fs::metadata(&full_path)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?
        .len();
    if let Some(range) = request
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, file_size))
    {
        let (start, end) = range;
        let chunk_size = end - start + 1;
        let data = read_file_range(&full_path, start, chunk_size).await?;
        return Ok(cot::http::Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, mime)
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{file_size}"))
            .header(CONTENT_LENGTH, chunk_size.to_string())
            .body(Body::fixed(data))
            .expect("valid response"));
    }
    let data = tokio::fs::read(full_path)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;
    Ok(cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, file_size.to_string())
        .body(Body::fixed(data))
        .expect("valid response"))
}

async fn local_upload_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    config: AppConfig,
    scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
    request: cot::request::Request,
) -> cot::Result<cot::http::Response<Body>> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let inbox_dir = config.agent_inbox_dir.trim();
    if inbox_dir.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "agent_inbox_dir is not configured",
        ));
    }
    let inbox_root = crate::media_paths::resolve_config_path_buf(inbox_dir);
    if !inbox_root.is_absolute() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "agent_inbox_dir must be an absolute path",
        ));
    }

    let filename_header = HeaderName::from_static("x-furumusic-filename");
    let original_name = request
        .headers()
        .get(filename_header)
        .and_then(|value| value.to_str().ok())
        .map(percent_decode_header)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "upload.mp3".to_string());
    let filename = sanitize_upload_filename(&original_name);

    let bytes = request
        .into_body()
        .into_bytes()
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;
    if bytes.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "uploaded file is empty",
        ));
    }

    let upload_dir = inbox_root
        .join("user_uploads")
        .join(user.id.to_string())
        .join(format!("local-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&upload_dir)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;
    let destination = upload_dir.join(&filename);
    tokio::fs::write(&destination, &bytes)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;

    if let Some(handle) = scheduler_handle.get() {
        let handle = Arc::clone(handle);
        tokio::spawn(async move {
            if let Err(err) = handle.trigger_job_now("inbox_discover").await {
                tracing::warn!("failed to trigger inbox_discover after local upload: {err}");
            }
        });
    }

    Json(LocalUploadResponse {
        ok: true,
        filename,
        size: bytes.len() as u64,
    })
    .into_response()
}

fn sanitize_upload_filename(value: &str) -> String {
    let name = std::path::Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.mp3");
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "upload.mp3".to_string()
    } else {
        trimmed.to_string()
    }
}

fn percent_decode_header(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = hex_value(bytes[index + 1]);
                let lo = hex_value(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_range(header: &str, file_size: u64) -> Option<(u64, u64)> {
    let bytes_prefix = "bytes=";
    if !header.starts_with(bytes_prefix) {
        return None;
    }
    let range_spec = &header[bytes_prefix.len()..];
    let parts: Vec<&str> = range_spec.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start: u64 = if parts[0].is_empty() {
        // Suffix range: bytes=-N means last N bytes
        let suffix: u64 = parts[1].parse().ok()?;
        file_size.saturating_sub(suffix)
    } else {
        parts[0].parse().ok()?
    };

    let end: u64 = if parts[1].is_empty() || parts[0].is_empty() {
        file_size - 1
    } else {
        parts[1].parse::<u64>().ok()?.min(file_size - 1)
    };

    if start > end || start >= file_size {
        return None;
    }

    Some((start, end))
}

async fn read_file_range(path: &std::path::Path, start: u64, length: u64) -> cot::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    Ok(buf)
}

// ---------------------------------------------------------------------------
// GET /api/player/cover/{media_file_id}
// ---------------------------------------------------------------------------

async fn cover_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    path: Path<PathMediaFileId>,
) -> cot::Result<cot::http::Response<Body>> {
    cover_response(
        auth_ctx,
        session,
        db,
        pool,
        config,
        path.0.media_file_id,
        None,
    )
    .await
}

async fn cover_variant_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    path: Path<PathMediaFileVariant>,
) -> cot::Result<cot::http::Response<Body>> {
    cover_response(
        auth_ctx,
        session,
        db,
        pool,
        config,
        path.0.media_file_id,
        Some(path.0.variant.as_str()),
    )
    .await
}

async fn cover_response(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    media_file_id: i64,
    variant_name: Option<&str>,
) -> cot::Result<cot::http::Response<Body>> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let media = sqlx::query_as::<_, MediaFileRow>(
        r#"SELECT file_path,
                  mime_type::text as mime_type,
                  file_size_bytes,
                  sha256_hash::text AS sha256_hash,
                  created_at::text AS created_at
           FROM furumusic__media_file WHERE id = $1"#,
    )
    .bind(media_file_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(media) = media else {
        return Ok(json_error(StatusCode::NOT_FOUND, "media file not found"));
    };

    let full_path =
        crate::media_paths::resolve_media_file_path(&config.agent_storage_dir, &media.file_path);

    if !full_path.exists() {
        return Ok(json_error(StatusCode::NOT_FOUND, "file not found on disk"));
    }

    let (response_path, content_type, etag_variant) = variant_name
        .and_then(crate::agent::cover_variants::variant_by_name)
        .map(|variant| {
            let variant_path = crate::agent::cover_variants::variant_path(&full_path, variant);
            if variant_path.exists() {
                (variant_path, "image/jpeg", Some(variant.name))
            } else {
                (full_path.clone(), media.mime_type.as_str(), None)
            }
        })
        .unwrap_or_else(|| (full_path.clone(), media.mime_type.as_str(), None));

    let data = tokio::fs::read(&response_path)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let etag = media_etag("cover", media_file_id, &media.sha256_hash, etag_variant);

    let response = cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, data.len().to_string())
        .header("ETag", etag.as_str())
        .header("X-Furumi-Content-Sha256", media.sha256_hash.as_str())
        .header("X-Furumi-Content-Updated-At", media.created_at.as_str())
        .header("Cache-Control", "public, max-age=86400")
        .body(Body::fixed(data))
        .expect("valid response");

    Ok(response)
}

fn media_etag(kind: &str, id: i64, sha256_hash: &str, variant: Option<&str>) -> String {
    match variant {
        Some(variant) => format!("\"{kind}-{id}-{variant}-{sha256_hash}\""),
        None => format!("\"{kind}-{id}-{sha256_hash}\""),
    }
}

// ---------------------------------------------------------------------------
// Player devices
// ---------------------------------------------------------------------------

async fn devices_heartbeat_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<DeviceHeartbeatRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    let response = hub.heartbeat(
        user.id,
        &device_id,
        dto.user_agent.as_deref(),
        dto.current_jam_id
            .as_deref()
            .and_then(normalize_device_id)
            .as_deref(),
        dto.playback_state,
    );
    Json(response).into_response()
}

async fn devices_poll_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<DeviceHeartbeatRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    let response = hub.poll(
        user.id,
        &device_id,
        dto.user_agent.as_deref(),
        dto.current_jam_id
            .as_deref()
            .and_then(normalize_device_id)
            .as_deref(),
        dto.playback_state,
    );
    Json(response).into_response()
}

async fn devices_select_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<DeviceSelectRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(target_device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };
    let current_device_id = dto
        .current_device_id
        .as_deref()
        .and_then(normalize_device_id)
        .unwrap_or_else(|| target_device_id.clone());
    let previous_active_id = hub.active_device_id_for_commands(user.id);

    let Some(response) = hub.select(user.id, &current_device_id, &target_device_id) else {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "target device is offline",
        ));
    };
    if let Some(fed_device_id) = fed_device_id_from_virtual(&target_device_id) {
        let Some(playback_state) = response.playback_state.clone() else {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "no playback state to transfer",
            ));
        };
        let state = match serde_json::to_value(playback_state) {
            Ok(state) => state,
            Err(err) => return Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
        };
        let previous_fed_device_id = previous_active_id
            .as_deref()
            .and_then(fed_device_id_from_virtual)
            .filter(|previous| *previous != fed_device_id);
        if let Err(err) = crate::federation::handle()
            .fed_device_web_active_transfer(user.id, fed_device_id, previous_fed_device_id, state)
            .await
        {
            return Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}")));
        }
    } else if let Some(previous_fed_device_id) = previous_active_id
        .as_deref()
        .and_then(fed_device_id_from_virtual)
        && let Some(playback_state) = response.playback_state.clone()
    {
        let state = match serde_json::to_value(playback_state) {
            Ok(state) => state,
            Err(err) => return Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
        };
        if let Err(err) = crate::federation::handle()
            .fed_device_web_active_takeover(user.id, previous_fed_device_id, state)
            .await
        {
            return Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}")));
        }
    }
    Json(response).into_response()
}

async fn devices_command_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<DeviceCommandRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let command = dto.command.trim();
    if command.is_empty() || command.len() > 64 {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid command"));
    }
    let target_device_id = match dto.target_device_id.as_deref() {
        Some(raw) => {
            let Some(device_id) = normalize_device_id(raw) else {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid target device id",
                ));
            };
            Some(device_id)
        }
        None => None,
    };
    let jam_id = match dto.jam_id.as_deref() {
        Some(raw) => {
            let Some(jam_id) = normalize_device_id(raw) else {
                return Ok(json_error(StatusCode::BAD_REQUEST, "invalid jam id"));
            };
            Some(jam_id)
        }
        None => None,
    };

    let mut payload = dto.payload;
    if jam_id.is_some() && matches!(command, "queue_add_end" | "queue_add_next") {
        stamp_jam_queue_tracks(&mut payload, user.id, &user.name);
    }

    if jam_id.is_none()
        && let Some(fed_device_id) = target_device_id
            .as_deref()
            .and_then(fed_device_id_from_virtual)
    {
        let current_state = hub.playback_state_json_for_commands(user.id);
        match crate::federation::handle()
            .fed_device_web_command(user.id, fed_device_id, command, payload, current_state)
            .await
        {
            Ok(next_state) => {
                let name = hub
                    .fed_device_name_for_commands(user.id, fed_device_id)
                    .unwrap_or_else(|| fed_device_id.to_string());
                let _ = hub.apply_fed_playback_state_json(
                    user.id,
                    fed_device_id,
                    &name,
                    true,
                    next_state,
                );
                return Json(serde_json::json!({"ok": true})).into_response();
            }
            Err(err) => return Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
        }
    }

    match hub.enqueue_command(
        user.id,
        target_device_id.as_deref(),
        jam_id.as_deref(),
        command,
        payload,
    ) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(message) => Ok(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

async fn fed_devices_status_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_status(user.id, &user.name)
        .await
    {
        Ok(status) => Json(status).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

async fn fed_devices_invite_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_invite(user.id, &user.name)
        .await
    {
        Ok(invite) => Json(serde_json::json!({ "invite": invite })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

async fn fed_devices_connect_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    Json(dto): Json<FedDeviceConnectRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_connect(user.id, &user.name, dto.invite.trim())
        .await
    {
        Ok(message) => Json(serde_json::json!({ "ok": true, "message": message })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

async fn fed_devices_pairing_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    Json(dto): Json<FedDevicePairingAnswerRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_answer_pairing(
            user.id,
            dto.request_id.trim(),
            dto.accept,
            dto.use_requester_group,
        )
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

async fn fed_devices_revoke_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    Json(dto): Json<FedDeviceRevokeRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_revoke(user.id, dto.device_id.trim())
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

async fn fed_devices_sync_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .fed_device_sync_now(user.id)
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err}"))),
    }
}

fn stamp_jam_queue_tracks(payload: &mut serde_json::Value, user_id: i64, user_name: &str) {
    let Some(tracks) = payload
        .get_mut("tracks")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for track in tracks {
        let Some(track_object) = track.as_object_mut() else {
            continue;
        };
        track_object.insert(
            "added_by_user_id".to_string(),
            serde_json::Value::Number(user_id.into()),
        );
        track_object.insert(
            "added_by_user_name".to_string(),
            serde_json::Value::String(user_name.to_string()),
        );
    }
}

async fn record_fed_track_like(pool: &sqlx::PgPool, user_id: i64, track_id: i64, liked: bool) {
    if let Err(err) =
        crate::federation::devices::record_track_like(pool, user_id, track_id, liked).await
    {
        tracing::warn!(
            track_id,
            liked,
            "fed device like op was not recorded: {err:#}"
        );
    }
}

async fn record_fed_playlist_created(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
) {
    if let Err(err) =
        crate::federation::devices::record_playlist_created(pool, user_id, playlist_id, title).await
    {
        tracing::warn!(
            playlist_id,
            "fed device playlist create op was not recorded: {err:#}"
        );
    }
}

async fn record_fed_playlist_renamed(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
) {
    if let Err(err) =
        crate::federation::devices::record_playlist_renamed(pool, user_id, playlist_id, title).await
    {
        tracing::warn!(
            playlist_id,
            "fed device playlist rename op was not recorded: {err:#}"
        );
    }
}

async fn record_fed_playlist_deleted(pool: &sqlx::PgPool, user_id: i64, playlist_id: i64) {
    if let Err(err) =
        crate::federation::devices::record_playlist_deleted(pool, user_id, playlist_id).await
    {
        tracing::warn!(
            playlist_id,
            "fed device playlist delete op was not recorded: {err:#}"
        );
    }
}

async fn record_fed_playlist_tracks_added(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    track_ids: &[i64],
) {
    if let Err(err) = crate::federation::devices::record_playlist_tracks_added(
        pool,
        user_id,
        playlist_id,
        track_ids,
    )
    .await
    {
        tracing::warn!(
            playlist_id,
            "fed device playlist add op was not recorded: {err:#}"
        );
    }
}

async fn record_fed_playlist_tracks_removed(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    content_ids: &[String],
) {
    if let Err(err) = crate::federation::devices::record_playlist_tracks_removed(
        pool,
        user_id,
        playlist_id,
        content_ids,
    )
    .await
    {
        tracing::warn!(
            playlist_id,
            "fed device playlist remove op was not recorded: {err:#}"
        );
    }
}

async fn jam_users_search_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    UrlQuery(query): UrlQuery<JamUserSearchQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let q = query.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Json(Vec::<PlayerJamUserDto>::new()).into_response();
    }
    let limit = query.limit.unwrap_or(10).clamp(1, 20);
    let pattern = format!("%{q}%");
    let prefix = format!("{q}%");

    let rows = sqlx::query_as::<_, PlayerJamUserRow>(
        r#"SELECT id, username::text AS username, display_name, email
           FROM furumusic__user
           WHERE is_active = true
             AND id <> $1
             AND (
                 username ILIKE $2
                 OR COALESCE(display_name, '') ILIKE $2
                 OR COALESCE(email, '') ILIKE $2
             )
           ORDER BY
             CASE
               WHEN username ILIKE $3 THEN 0
               WHEN COALESCE(display_name, '') ILIKE $3 THEN 1
               ELSE 2
             END,
             COALESCE(NULLIF(display_name, ''), username)
           LIMIT $4"#,
    )
    .bind(user.id)
    .bind(pattern)
    .bind(prefix)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let users = rows
        .into_iter()
        .map(|row| PlayerJamUserDto {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
            email: row.email,
        })
        .collect::<Vec<_>>();

    Json(users).into_response()
}

async fn jam_create_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<PlayerJamCreateRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    let invitees = load_jam_invitees(pool, user.id, dto.invitee_user_ids).await?;

    match hub.create_jam(user.id, &user.name, &device_id, invitees) {
        Ok(response) => Json(response).into_response(),
        Err(message) => Ok(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

async fn load_jam_invitees(
    pool: &sqlx::PgPool,
    current_user_id: i64,
    invitee_user_ids: Vec<i64>,
) -> cot::Result<Vec<(i64, String)>> {
    let mut invitee_ids = invitee_user_ids
        .into_iter()
        .filter(|id| *id > 0 && *id != current_user_id)
        .collect::<Vec<_>>();
    invitee_ids.sort_unstable();
    invitee_ids.dedup();
    invitee_ids.truncate(PLAYER_JAM_MAX_INVITEES);

    if invitee_ids.is_empty() {
        Ok(Vec::new())
    } else {
        let invitees = sqlx::query_as::<_, PlayerJamUserRow>(
            r#"SELECT id, username::text AS username, display_name, email
               FROM furumusic__user
               WHERE is_active = true AND id = ANY($1)"#,
        )
        .bind(&invitee_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
        .into_iter()
        .map(|row| {
            let name = row
                .display_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&row.username)
                .to_string();
            (row.id, name)
        })
        .collect::<Vec<_>>();
        Ok(invitees)
    }
}

async fn jam_join_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<PlayerJamJoinRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(jam_id) = normalize_device_id(&dto.jam_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid jam id"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    match hub.join_jam(user.id, &user.name, &device_id, &jam_id) {
        Ok(response) => Json(response).into_response(),
        Err(message) => Ok(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

async fn jam_invite_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<PlayerJamInviteRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(jam_id) = normalize_device_id(&dto.jam_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid jam id"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    let invitees = load_jam_invitees(pool, user.id, dto.invitee_user_ids).await?;
    match hub.invite_to_jam(user.id, &device_id, &jam_id, invitees) {
        Ok(response) => Json(response).into_response(),
        Err(message) => Ok(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

async fn jam_leave_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    hub: Arc<PlayerDeviceHub>,
    Json(dto): Json<PlayerJamLeaveRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(jam_id) = normalize_device_id(&dto.jam_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid jam id"));
    };
    let Some(device_id) = normalize_device_id(&dto.device_id) else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid device id"));
    };

    match hub.leave_jam(user.id, &device_id, &jam_id) {
        Ok(response) => Json(response).into_response(),
        Err(message) => Ok(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

// ---------------------------------------------------------------------------
// GET /api/player/state
// ---------------------------------------------------------------------------

async fn get_state_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let state = sqlx::query_as::<_, PlaybackStateRow>(
        r#"SELECT current_track_id, position_ms, queue_json, queue_position, shuffle, repeat_mode::text as repeat_mode, volume
           FROM furumusic__playback_state WHERE user_id = $1"#,
    )
    .bind(user.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let dto = match state {
        Some(s) => {
            let queue: Vec<i64> = serde_json::from_str(&s.queue_json).unwrap_or_default();
            PlaybackStateDto {
                current_track_id: s.current_track_id,
                position_ms: s.position_ms,
                queue,
                queue_position: s.queue_position,
                shuffle: s.shuffle,
                repeat_mode: s.repeat_mode,
                volume: s.volume,
            }
        }
        None => PlaybackStateDto {
            current_track_id: None,
            position_ms: 0,
            queue: Vec::new(),
            queue_position: 0,
            shuffle: false,
            repeat_mode: "off".to_string(),
            volume: 0.7,
        },
    };

    Json(dto).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/player/state
// ---------------------------------------------------------------------------

async fn put_state_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(dto): Json<PlaybackStateDto>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let queue_json =
        serde_json::to_string(&dto.queue).map_err(|e| cot::Error::internal(e.to_string()))?;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    sqlx::query(
        r#"INSERT INTO furumusic__playback_state (user_id, current_track_id, position_ms, queue_json, queue_position, shuffle, repeat_mode, volume, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           ON CONFLICT (user_id) DO UPDATE SET
             current_track_id = $2, position_ms = $3, queue_json = $4,
             queue_position = $5, shuffle = $6, repeat_mode = $7, volume = $8, updated_at = $9"#,
    )
    .bind(user.id)
    .bind(dto.current_track_id)
    .bind(dto.position_ms)
    .bind(&queue_json)
    .bind(dto.queue_position)
    .bind(dto.shuffle)
    .bind(&dto.repeat_mode)
    .bind(dto.volume)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/player/history
// ---------------------------------------------------------------------------

async fn history_list_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    query: cot::request::extractors::UrlQuery<HistoryQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let page = query.0.page.unwrap_or(1).max(1);
    let per_page = query.0.limit.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) as i64 * per_page as i64;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__listen_event
          WHERE user_id = $1 AND qualified = true",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let rows = sqlx::query(
        "SELECT le.listen_id, le.content_id,
                COALESCE(le.local_track_id, tr.local_track_id) AS local_track_id,
                le.origin_device_id, le.started_at_ms, le.listened_ms,
                le.track_duration_ms, le.metadata_json,
                COALESCE(NULLIF(d.name, ''), le.origin_device_id) AS device_name,
                t.release_id, r.year AS release_year, t.cover_file_id,
                r.cover_file_id AS release_cover_file_id
           FROM furumusic__listen_event le
           LEFT JOIN furumusic__fed_device d
             ON d.user_id = le.user_id AND d.device_id = le.origin_device_id
           LEFT JOIN furumusic__track_ref tr ON tr.content_id = le.content_id
           LEFT JOIN furumusic__track t
             ON t.id = COALESCE(le.local_track_id, tr.local_track_id)
           LEFT JOIN furumusic__release r ON r.id = t.release_id
          WHERE le.user_id = $1 AND le.qualified = true
          ORDER BY le.started_at_ms DESC, le.listen_id DESC
          LIMIT $2 OFFSET $3",
    )
    .bind(user.id)
    .bind(i64::from(per_page))
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let items = rows
        .into_iter()
        .map(|row| {
            let listen_id: String = row.get("listen_id");
            let content_id: String = row.get("content_id");
            let local_track_id: Option<i64> = row.get("local_track_id");
            let metadata: serde_json::Value = row.get("metadata_json");
            let title = metadata
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Unknown track")
                .to_string();
            let release_title = metadata
                .get("release_title")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let artist_values = |key: &str| {
                metadata
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(|name| serde_json::json!({"id": null, "name": name}))
                    .collect::<Vec<_>>()
            };
            let duration_ms: Option<i64> = row.get("track_duration_ms");
            let release_id: Option<i64> = row.get("release_id");
            let cover_url = track_cover_variant_url(
                row.get("cover_file_id"),
                row.get("release_cover_file_id"),
                "medium",
            );
            let track = serde_json::json!({
                "id": local_track_id.map_or_else(
                    || format!("history:{content_id}"),
                    |id| id.to_string(),
                ),
                "content_id": content_id,
                "title": title,
                "track_number": null,
                "disc_number": null,
                "duration_seconds": duration_ms.unwrap_or_default() as f64 / 1000.0,
                "artists": artist_values("artist_names"),
                "featured_artists": artist_values("featured_artist_names"),
                "release_id": release_id,
                "release_title": release_title.clone().unwrap_or_default(),
                "release_year": row.get::<Option<i32>, _>("release_year"),
                "cover_url": cover_url,
                "stream_url": local_track_id
                    .map(|id| format!("/api/player/stream/{id}"))
                    .unwrap_or_default(),
                "uploader_name": if local_track_id.is_some() { "Web" } else { "Federation" },
                "federation_pending": local_track_id.is_none(),
                "_federationTrack": if local_track_id.is_none() {
                    Some(serde_json::json!({
                        "key": {"content_id": content_id},
                        "metadata": metadata,
                        "availability": {
                            "state": "federated",
                            "local": null,
                            "federation": [{"owner": "", "item_id": ""}],
                        },
                    }))
                } else {
                    None
                },
            });
            let started_at_ms: i64 = row.get("started_at_ms");
            let played_at = chrono::DateTime::from_timestamp_millis(started_at_ms)
                .unwrap_or_else(chrono::Utc::now)
                .to_rfc3339();
            PlayHistoryItem {
                id: listen_id,
                track_id: local_track_id,
                track_title: title,
                release_title,
                track,
                played_at,
                duration_listened: Some(
                    row.get::<i64, _>("listened_ms")
                        .div_euclid(1_000)
                        .clamp(0, i64::from(i32::MAX)) as i32,
                ),
                completed: true,
                device_id: row.get("origin_device_id"),
                device_name: row.get("device_name"),
            }
        })
        .collect();

    Json(PlayHistoryPage {
        items,
        total,
        page,
        per_page,
    })
    .into_response()
}

async fn history_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(entry): Json<HistoryEntry>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let listened_seconds = entry.duration_listened.unwrap_or_default().max(0);
    let started_at = entry
        .started_at
        .unwrap_or_else(|| chrono::Utc::now().timestamp() - i64::from(listened_seconds));
    let listen_id = entry
        .listen_id
        .unwrap_or_else(|| format!("legacy-web:{}:{}:{started_at}", user.id, entry.track_id));
    let ended_reason = match entry.ended_reason.as_deref() {
        Some("finished") => music_dht::device_sync::ListenEndReason::Finished,
        Some("skipped") => music_dht::device_sync::ListenEndReason::Skipped,
        Some("replaced") => music_dht::device_sync::ListenEndReason::Replaced,
        Some("stopped") => music_dht::device_sync::ListenEndReason::Stopped,
        _ => music_dht::device_sync::ListenEndReason::Unknown,
    };
    crate::federation::devices::record_track_listen(
        pool,
        user.id,
        entry.track_id,
        listen_id,
        started_at.saturating_mul(1_000),
        i64::from(listened_seconds).saturating_mul(1_000),
        ended_reason,
    )
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    crate::metrics::record_play_history(entry.duration_listened, entry.completed);

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/search?q=...&limit=N
// ---------------------------------------------------------------------------

async fn search_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    query: cot::request::extractors::UrlQuery<SearchQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let q = query.0.q.trim().to_lowercase();
    if q.is_empty() {
        return Json(SearchResults {
            artists: Vec::new(),
            releases: Vec::new(),
            tracks: Vec::new(),
        })
        .into_response();
    }
    let normalized_q = match music_dht::normalize_name(&q) {
        normalized if !normalized.is_empty() => normalized,
        _ => q.clone(),
    };

    let limit = query.0.limit.unwrap_or(10).clamp(1, 50) as i64;
    let short = q.chars().count() < 3;

    let (artist_rows, release_rows, track_rows) = if short {
        let a = sqlx::query_as::<_, SearchArtistRow>(
            r#"SELECT a.id, a.name::text AS name, a.image_file_id,
                      COALESCE((SELECT COUNT(*) FROM furumusic__release_artist ra
                                JOIN furumusic__release r ON r.id = ra.release_id AND r.is_hidden = false
                                WHERE ra.artist_id = a.id), 0) AS release_count,
                      COALESCE((SELECT COUNT(*) FROM furumusic__release_artist ra
                                JOIN furumusic__release r ON r.id = ra.release_id AND r.is_hidden = false
                                JOIN furumusic__track t ON t.release_id = r.id AND t.is_hidden = false
                                WHERE ra.artist_id = a.id), 0) AS track_count
               FROM furumusic__artist a
               WHERE a.is_hidden = false
                 AND (a.name_sort ILIKE '%' || $1 || '%'
                      OR a.name_sort ILIKE '%' || $3 || '%')
               ORDER BY a.name_sort LIMIT $2"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        let r = sqlx::query_as::<_, SearchReleaseRow>(
            r#"SELECT r.id, r.title::text AS title, r.release_type::text AS release_type,
                      r.year, r.cover_file_id,
                      COALESCE((SELECT COUNT(*) FROM furumusic__track t WHERE t.release_id = r.id AND t.is_hidden = false), 0) AS track_count
               FROM furumusic__release r
               WHERE r.is_hidden = false
                 AND (r.title_sort ILIKE '%' || $1 || '%'
                      OR r.title_sort ILIKE '%' || $3 || '%')
               ORDER BY r.title_sort LIMIT $2"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        let t = sqlx::query_as::<_, SearchTrackRow>(
            r#"SELECT t.id, t.title::text AS title, t.track_number, t.disc_number,
                      t.duration_seconds, t.cover_file_id,
                      rel.cover_file_id AS release_cover_file_id,
                      rel.id AS release_id,
                      rel.title::text AS release_title,
                      rel.year AS release_year,
                      COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                      mf.audio_format,
                      mf.audio_bitrate,
                      mf.audio_sample_rate,
                      mf.audio_bit_depth,
                      mf.file_size_bytes,
                  t.lastfm_listeners,
                  t.lastfm_playcount,
                  t.lastfm_rating,
                  t.lastfm_updated_at
               FROM furumusic__track t
               JOIN furumusic__release rel ON rel.id = t.release_id
               LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
               WHERE t.is_hidden = false
                 AND (t.title_sort ILIKE '%' || $1 || '%'
                      OR t.title_sort ILIKE '%' || $3 || '%')
               ORDER BY t.title_sort LIMIT $2"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        tokio::try_join!(a, r, t).map_err(|e| cot::Error::internal(e.to_string()))?
    } else {
        let a = sqlx::query_as::<_, SearchArtistRow>(
            r#"SELECT id, name, image_file_id, release_count, track_count FROM (
                SELECT a.id, a.name::text AS name, a.image_file_id,
                       COALESCE((SELECT COUNT(*) FROM furumusic__release_artist ra
                                 JOIN furumusic__release r ON r.id = ra.release_id AND r.is_hidden = false
                                 WHERE ra.artist_id = a.id), 0) AS release_count,
                       COALESCE((SELECT COUNT(*) FROM furumusic__release_artist ra
                                 JOIN furumusic__release r ON r.id = ra.release_id AND r.is_hidden = false
                                 JOIN furumusic__track t ON t.release_id = r.id AND t.is_hidden = false
                                 WHERE ra.artist_id = a.id), 0) AS track_count,
                       MAX(sim) AS similarity
                FROM (
                    SELECT id, name, image_file_id, name_sort,
                           GREATEST(similarity(name_sort, $1), similarity(name_sort, $3)) AS sim
                    FROM furumusic__artist
                    WHERE is_hidden = false AND (name_sort % $1 OR name_sort % $3)
                    UNION ALL
                    SELECT id, name, image_file_id, name_sort, 0.01::real AS sim
                    FROM furumusic__artist
                    WHERE is_hidden
                      = false AND (name_sort ILIKE '%' || $1 || '%'
                                   OR name_sort ILIKE '%' || $3 || '%')
                ) a
                GROUP BY a.id, a.name, a.image_file_id
                ORDER BY similarity DESC
                LIMIT $2
            ) sub"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        let r = sqlx::query_as::<_, SearchReleaseRow>(
            r#"SELECT id, title, release_type, year, cover_file_id, track_count FROM (
                SELECT r.id, r.title::text AS title, r.release_type::text AS release_type,
                       r.year, r.cover_file_id,
                       COALESCE((SELECT COUNT(*) FROM furumusic__track t WHERE t.release_id = r.id AND t.is_hidden = false), 0) AS track_count,
                       MAX(sim) AS similarity
                FROM (
                    SELECT id, title, release_type, year, cover_file_id, title_sort,
                           GREATEST(similarity(title_sort, $1), similarity(title_sort, $3)) AS sim
                    FROM furumusic__release
                    WHERE is_hidden = false AND (title_sort % $1 OR title_sort % $3)
                    UNION ALL
                    SELECT id, title, release_type, year, cover_file_id, title_sort, 0.01::real AS sim
                    FROM furumusic__release
                    WHERE is_hidden = false
                      AND (title_sort ILIKE '%' || $1 || '%'
                           OR title_sort ILIKE '%' || $3 || '%')
                ) r
                GROUP BY r.id, r.title, r.release_type, r.year, r.cover_file_id
                ORDER BY similarity DESC
                LIMIT $2
            ) sub"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        let t = sqlx::query_as::<_, SearchTrackRow>(
            r#"SELECT id, title, track_number, disc_number, duration_seconds, cover_file_id,
                      release_cover_file_id, release_id, release_title, release_year, uploader_name, audio_format, audio_bitrate,
                      audio_sample_rate, audio_bit_depth, file_size_bytes, lastfm_listeners, lastfm_playcount, lastfm_rating, lastfm_updated_at FROM (
                SELECT t.id, t.title::text AS title, t.track_number, t.disc_number,
                       t.duration_seconds, t.cover_file_id,
                       rel.cover_file_id AS release_cover_file_id,
                       rel.id AS release_id,
                       rel.title::text AS release_title,
                       rel.year AS release_year,
                       COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                       mf.audio_format,
                       mf.audio_bitrate,
                       mf.audio_sample_rate,
                       mf.audio_bit_depth,
                       mf.file_size_bytes,
                       t.lastfm_listeners,
                       t.lastfm_playcount,
                       t.lastfm_rating,
                       t.lastfm_updated_at,
                       MAX(sim) AS similarity
                FROM (
                    SELECT id, title, title_sort, track_number, disc_number, duration_seconds, cover_file_id, release_id, audio_file_id,
                           lastfm_listeners, lastfm_playcount, lastfm_rating, lastfm_updated_at,
                           GREATEST(similarity(title_sort, $1), similarity(title_sort, $3)) AS sim
                    FROM furumusic__track
                    WHERE is_hidden = false AND (title_sort % $1 OR title_sort % $3)
                    UNION ALL
                    SELECT id, title, title_sort, track_number, disc_number, duration_seconds, cover_file_id, release_id, audio_file_id,
                           lastfm_listeners, lastfm_playcount, lastfm_rating, lastfm_updated_at,
                           0.01::real AS sim
                    FROM furumusic__track
                    WHERE is_hidden = false
                      AND (title_sort ILIKE '%' || $1 || '%'
                           OR title_sort ILIKE '%' || $3 || '%')
                ) t
                JOIN furumusic__release rel ON rel.id = t.release_id
                LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
                GROUP BY t.id, t.title, t.track_number, t.disc_number, t.duration_seconds, t.cover_file_id, rel.cover_file_id, rel.id, rel.title, rel.year,
                         mf.uploader_name, mf.audio_format, mf.audio_bitrate, mf.audio_sample_rate, mf.audio_bit_depth, mf.file_size_bytes,
                         t.lastfm_listeners, t.lastfm_playcount, t.lastfm_rating, t.lastfm_updated_at
                ORDER BY similarity DESC
                LIMIT $2
            ) sub"#,
        )
        .bind(&q)
        .bind(limit)
        .bind(&normalized_q)
        .fetch_all(pool);

        tokio::try_join!(a, r, t).map_err(|e| cot::Error::internal(e.to_string()))?
    };

    // Collect track IDs for batch artist lookup
    let track_ids: Vec<i64> = track_rows.iter().map(|t| t.id).collect();

    let track_artists = if track_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, TrackArtistRow>(
            r#"SELECT ta.track_id, ta.artist_id, a.name::text AS artist_name, ta.role::text AS role
               FROM furumusic__track_artist ta
               JOIN furumusic__artist a ON a.id = ta.artist_id
               WHERE ta.track_id = ANY($1)
               ORDER BY ta.track_id, ta.position"#,
        )
        .bind(&track_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    };

    let mut track_main_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();
    let mut track_feat_artists: std::collections::HashMap<i64, Vec<ArtistRef>> =
        std::collections::HashMap::new();

    for ta in &track_artists {
        let artist_ref = ArtistRef {
            id: ta.artist_id,
            name: ta.artist_name.clone(),
        };
        if ta.role == "featuring" {
            track_feat_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        } else {
            track_main_artists
                .entry(ta.track_id)
                .or_default()
                .push(artist_ref);
        }
    }

    let artists: Vec<ArtistCard> = artist_rows
        .into_iter()
        .map(|r| ArtistCard {
            id: r.id,
            name: r.name,
            image_url: cover_variant_url(r.image_file_id, "medium"),
            release_count: r.release_count,
            track_count: r.track_count,
        })
        .collect();

    let release_ids: Vec<i64> = release_rows.iter().map(|r| r.id).collect();
    let mut release_uploaders = load_release_uploaders(pool, &release_ids)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let releases: Vec<ReleaseCard> = release_rows
        .into_iter()
        .map(|r| ReleaseCard {
            id: r.id,
            title: r.title,
            release_type: r.release_type,
            year: r.year,
            cover_url: cover_variant_url(r.cover_file_id, "medium"),
            track_count: r.track_count,
            uploaders: release_uploaders.remove(&r.id).unwrap_or_default(),
        })
        .collect();

    let tracks: Vec<TrackItem> = track_rows
        .into_iter()
        .map(|t| {
            let tid = t.id;
            TrackItem {
                id: t.id,
                content_id: None,
                title: t.title,
                track_number: t.track_number,
                disc_number: t.disc_number,
                duration_seconds: t.duration_seconds,
                artists: track_main_artists.remove(&tid).unwrap_or_default(),
                featured_artists: track_feat_artists.remove(&tid).unwrap_or_default(),
                release_id: t.release_id,
                release_title: t.release_title,
                release_year: t.release_year,
                cover_url: track_cover_variant_url(
                    t.cover_file_id,
                    t.release_cover_file_id,
                    "medium",
                ),
                stream_url: format!("/api/player/stream/{tid}"),
                uploader_name: t.uploader_name,
                audio_format: t.audio_format,
                audio_bitrate: t.audio_bitrate,
                audio_sample_rate: t.audio_sample_rate,
                audio_bit_depth: t.audio_bit_depth,
                file_size_bytes: t.file_size_bytes,
                lastfm_listeners: t.lastfm_listeners,
                lastfm_playcount: t.lastfm_playcount,
                lastfm_rating: t.lastfm_rating,
                lastfm_updated_at: t.lastfm_updated_at,
            }
        })
        .collect();

    Json(SearchResults {
        artists,
        releases,
        tracks,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/federation/search/events?q=...
// ---------------------------------------------------------------------------

async fn federation_search_events_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    query: cot::request::extractors::UrlQuery<SearchQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let q = query.0.q.trim().to_owned();
    if q.is_empty() || q.chars().count() > 200 {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid search query"));
    }

    let search_id = uuid::Uuid::new_v4().to_string();
    let body_search_id = search_id.clone();
    let body = Body::streaming(async_stream::try_stream! {
        let started = serde_json::json!({
            "search_id": body_search_id,
            "sequence": 0,
            "kind": "search.started",
            "entity_key": null,
            "entity": { "query": q },
        });
        yield Bytes::from(format!("event: search.started\ndata: {started}\n\n"));

        match crate::federation::handle().search_events(&body_search_id, &q).await {
            Ok(events) => {
                let mut last_sequence = 0;
                for event in events {
                    last_sequence = event.sequence;
                    let data = serde_json::to_string(&event)
                        .map_err(|err| cot::Error::internal(err.to_string()))?;
                    yield Bytes::from(format!("event: {}\ndata: {data}\n\n", event.kind));
                }
                let completed = serde_json::json!({
                    "search_id": body_search_id,
                    "sequence": last_sequence + 1,
                    "kind": "search.completed",
                    "entity_key": null,
                    "entity": { "partial": false },
                });
                yield Bytes::from(format!("event: search.completed\ndata: {completed}\n\n"));
            }
            Err(err) => {
                let failed = serde_json::json!({
                    "search_id": body_search_id,
                    "sequence": 1,
                    "kind": "search.failed",
                    "entity_key": null,
                    "entity": { "message": err.to_string() },
                });
                yield Bytes::from(format!("event: search.failed\ndata: {failed}\n\n"));
            }
        }
    });
    cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(body)
        .map_err(|err| cot::Error::internal(err.to_string()))
}

async fn federation_artist_events_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    UrlQuery(query): UrlQuery<FederationArtistQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let name = query.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 200 {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid artist name"));
    }
    let mut catalogs = crate::federation::handle().stream_artist_catalogs(name.clone());
    let body = Body::streaming(async_stream::try_stream! {
        let started = serde_json::json!({ "kind": "artist.started", "entity": { "name": name } });
        yield Bytes::from(format!("event: artist.started\ndata: {started}\n\n"));
        let mut sequence = 0u64;
        while let Some(result) = catalogs.recv().await {
            sequence += 1;
            match result {
                Ok((owner, artist)) => {
                    let event = serde_json::json!({
                        "kind": "federation.artist.catalog",
                        "sequence": sequence,
                        "peer": owner,
                        "entity": artist,
                    });
                    yield Bytes::from(format!("event: federation.artist.catalog\ndata: {event}\n\n"));
                }
                Err(err) => {
                    tracing::debug!(artist = %name, "federated artist catalog skipped: {err:#}");
                }
            }
        }
        let completed = serde_json::json!({
            "kind": "artist.completed",
            "sequence": sequence + 1,
            "entity": { "partial": false },
        });
        yield Bytes::from(format!("event: artist.completed\ndata: {completed}\n\n"));
    });
    cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(body)
        .map_err(|err| cot::Error::internal(err.to_string()))
}

async fn content_like_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<ContentTrackMutation>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let liked = body.liked.unwrap_or(true);
    crate::federation::devices::record_content_like(
        pool,
        user.id,
        &body.content_id,
        liked,
        body.federation,
    )
    .await
    .map_err(|err| cot::Error::internal(err.to_string()))?;
    Json(serde_json::json!({ "ok": true, "liked": liked })).into_response()
}

async fn content_likes_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT content_id FROM furumusic__fed_state_like
          WHERE user_id = $1 AND liked = true ORDER BY hlc_ms DESC",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|err| cot::Error::internal(err.to_string()))?;
    Json(ids).into_response()
}

async fn content_playlist_add_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<ContentTrackMutation>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let Some(playlist_id) = body.playlist_id else {
        return Ok(json_error(StatusCode::BAD_REQUEST, "missing playlist id"));
    };
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM furumusic__playlist WHERE id = $1 AND owner_id = $2
         )",
    )
    .bind(playlist_id)
    .bind(user.id)
    .fetch_one(pool)
    .await
    .map_err(|err| cot::Error::internal(err.to_string()))?;
    if !owns {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }
    let position = match body.position {
        Some(position) => position,
        None => sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(position), -1) + 1
               FROM furumusic__fed_state_playlist_item i
               JOIN furumusic__fed_state_playlist p
                 ON p.user_id = i.user_id AND p.playlist_id = i.playlist_id
              WHERE p.user_id = $1 AND p.local_playlist_id = $2 AND i.present = true",
        )
        .bind(user.id)
        .bind(playlist_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0),
    };
    crate::federation::devices::record_content_playlist_add(
        pool,
        user.id,
        playlist_id,
        &body.content_id,
        position,
        body.federation,
    )
    .await
    .map_err(|err| cot::Error::internal(err.to_string()))?;
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn federation_playlist_tracks_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Path(path): Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let rows = if path.id == -1 {
        sqlx::query(
            "SELECT content_id, hlc_ms AS position, fed_json
               FROM furumusic__fed_state_like
              WHERE user_id = $1 AND liked = true
                AND local_track_id IS NULL AND fed_json IS NOT NULL
              ORDER BY hlc_ms DESC",
        )
        .bind(user.id)
        .fetch_all(pool)
        .await
    } else {
        let accessible: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM furumusic__playlist p
                 WHERE p.id = $1
                   AND (p.owner_id = $2 OR p.is_public = true OR EXISTS (
                       SELECT 1 FROM furumusic__saved_playlist sp
                        WHERE sp.user_id = $2 AND sp.playlist_id = p.id
                   ))
            )",
        )
        .bind(path.id)
        .bind(user.id)
        .fetch_one(pool)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;
        if !accessible {
            return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
        }
        sqlx::query(
            "SELECT i.content_id, i.position, i.fed_json
               FROM furumusic__fed_state_playlist_item i
               JOIN furumusic__fed_state_playlist p
                 ON p.user_id = i.user_id AND p.playlist_id = i.playlist_id
              WHERE i.user_id = $1 AND p.local_playlist_id = $2
                AND i.present = true AND i.local_track_id IS NULL
                AND i.fed_json IS NOT NULL
              ORDER BY i.position",
        )
        .bind(user.id)
        .bind(path.id)
        .fetch_all(pool)
        .await
    }
    .map_err(|err| cot::Error::internal(err.to_string()))?;
    let tracks = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "content_id": row.get::<String, _>("content_id"),
                "position": row.get::<i64, _>("position"),
                "federation": row.get::<serde_json::Value, _>("fed_json"),
            })
        })
        .collect::<Vec<_>>();
    Json(tracks).into_response()
}

async fn prepare_federated_track_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    Json(body): Json<PrepareFederatedTrackRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    tokio::spawn(async move {
        let federation = crate::federation::handle();
        let progress_sender = sender.clone();
        let result = match (body.owner.as_deref(), body.item_id.as_deref()) {
            (Some(owner), Some(item_id)) if !owner.is_empty() && !item_id.is_empty() => {
                federation
                    .prepare_content_with_progress(
                        &body.content_id,
                        owner,
                        item_id,
                        move |progress| {
                            let _ = progress_sender.send(serde_json::json!({
                                "kind": "progress",
                                "phase": progress.phase,
                                "received": progress.received,
                                "total": progress.total,
                            }));
                        },
                    )
                    .await
            }
            _ => {
                federation
                    .prepare_discovered_content_with_progress(&body.content_id, move |progress| {
                        let _ = progress_sender.send(serde_json::json!({
                            "kind": "progress",
                            "phase": progress.phase,
                            "received": progress.received,
                            "total": progress.total,
                        }));
                    })
                    .await
            }
        };
        let event = match result {
            Ok(prepared) => serde_json::json!({
                "kind": "completed",
                "ok": true,
                "local_track_id": prepared.local_track_id,
                "stream_url": prepared.stream_url,
            }),
            Err(err) => serde_json::json!({
                "kind": "failed",
                "error": err.to_string(),
            }),
        };
        let _ = sender.send(event);
    });
    let response_body = Body::streaming(async_stream::try_stream! {
        while let Some(event) = receiver.recv().await {
            let line = serde_json::to_string(&event)
                .map_err(|err| cot::Error::internal(err.to_string()))?;
            yield Bytes::from(format!("{line}\n"));
        }
    });
    cot::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/x-ndjson")
        .header("cache-control", "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(response_body)
        .map_err(|err| cot::Error::internal(err.to_string()))
}

async fn federation_track_artwork_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    UrlQuery(query): UrlQuery<FederationArtworkQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .track_artwork(&query.owner, &query.item_id)
        .await
    {
        Ok(Some((bytes, mime))) => Ok(cot::http::Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, mime)
            .header("cache-control", "private, max-age=86400")
            .body(Body::fixed(bytes))
            .expect("valid response")),
        Ok(None) => Ok(json_error(StatusCode::NOT_FOUND, "artwork not available")),
        Err(err) => Ok(json_error(StatusCode::BAD_GATEWAY, &err.to_string())),
    }
}

async fn federation_catalog_artwork_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    UrlQuery(query): UrlQuery<FederationCatalogArtworkQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .catalog_artwork(&query.owner, &query.artist, query.release.as_deref())
        .await
    {
        Ok(Some((bytes, mime))) => Ok(cot::http::Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, mime)
            .header("cache-control", "private, max-age=86400")
            .body(Body::fixed(bytes))
            .expect("valid response")),
        Ok(None) => Ok(json_error(StatusCode::NOT_FOUND, "artwork not available")),
        Err(err) => Ok(json_error(StatusCode::BAD_GATEWAY, &err.to_string())),
    }
}

async fn federation_artwork_discovery_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    UrlQuery(query): UrlQuery<FederationArtworkDiscoveryQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    match crate::federation::handle()
        .discover_catalog_artwork(&query.artist, query.release.as_deref())
        .await
    {
        Ok(Some((bytes, mime))) => Ok(cot::http::Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, mime)
            .header("cache-control", "private, max-age=300")
            .body(Body::fixed(bytes))
            .expect("valid response")),
        Ok(None) => Ok(json_error(StatusCode::NOT_FOUND, "artwork not available")),
        Err(err) => Ok(json_error(StatusCode::BAD_GATEWAY, &err.to_string())),
    }
}

// ---------------------------------------------------------------------------
// POST /api/player/playlists  — create playlist
// ---------------------------------------------------------------------------

async fn create_playlist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<CreatePlaylistRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "title is required"));
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO furumusic__playlist (owner_id, title, is_public, created_at, updated_at) \
         VALUES ($1, $2, false, $3, $3) RETURNING id",
    )
    .bind(user.id)
    .bind(&title)
    .bind(&now)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    record_fed_playlist_created(pool, user.id, row.0, &title).await;

    Json(PlaylistCard {
        id: row.0,
        title,
        track_count: 0,
        is_own: true,
        owner_name: Some(user.name),
        is_public: false,
        is_saved: false,
        kind: "user".to_string(),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/player/playlists/{id}  — rename / update playlist
// ---------------------------------------------------------------------------

async fn update_playlist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<UpdatePlaylistRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let playlist_id = path.0.id;
    // Verify ownership
    let owner: Option<(i64,)> =
        sqlx::query_as("SELECT owner_id FROM furumusic__playlist WHERE id = $1")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(owner) = owner else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    if owner.0 != user.id {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Some(title) = &body.title {
        let t = title.trim();
        if !t.is_empty() {
            sqlx::query("UPDATE furumusic__playlist SET title = $1, updated_at = $2 WHERE id = $3")
                .bind(t)
                .bind(&now)
                .bind(playlist_id)
                .execute(pool)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
            record_fed_playlist_renamed(pool, user.id, playlist_id, t).await;
        }
    }
    if let Some(desc) = &body.description {
        sqlx::query(
            "UPDATE furumusic__playlist SET description = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(desc)
        .bind(&now)
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// DELETE /api/player/playlists/{id}
// ---------------------------------------------------------------------------

async fn delete_playlist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let playlist_id = path.0.id;
    let owner: Option<(i64,)> =
        sqlx::query_as("SELECT owner_id FROM furumusic__playlist WHERE id = $1")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(owner) = owner else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    if owner.0 != user.id {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }
    record_fed_playlist_deleted(pool, user.id, playlist_id).await;
    sqlx::query("DELETE FROM furumusic__playlist_track WHERE playlist_id = $1")
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query("DELETE FROM furumusic__saved_playlist WHERE playlist_id = $1")
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query("DELETE FROM furumusic__playlist WHERE id = $1")
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/player/playlists/{id}/tracks  — add tracks to playlist
// ---------------------------------------------------------------------------

async fn add_tracks_to_playlist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<AddTracksRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let playlist_id = path.0.id;
    let owner: Option<(i64,)> =
        sqlx::query_as("SELECT owner_id FROM furumusic__playlist WHERE id = $1")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(owner) = owner else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    if owner.0 != user.id {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }

    // Get next position
    let max_pos: (Option<i32>,) = sqlx::query_as(
        "SELECT MAX(position) FROM furumusic__playlist_track WHERE playlist_id = $1",
    )
    .bind(playlist_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let mut pos = max_pos.0.unwrap_or(-1) + 1;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    for track_id in &body.track_ids {
        sqlx::query(
            "INSERT INTO furumusic__playlist_track (playlist_id, track_id, position, added_at, added_by_user_id) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(playlist_id)
        .bind(track_id)
        .bind(pos)
        .bind(&now)
        .bind(user.id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        pos += 1;
    }

    sqlx::query("UPDATE furumusic__playlist SET updated_at = $1 WHERE id = $2")
        .bind(&now)
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    record_fed_playlist_tracks_added(pool, user.id, playlist_id, &body.track_ids).await;

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/player/playlists/{id}/tracks  — reorder playlist tracks
// ---------------------------------------------------------------------------

async fn reorder_playlist_tracks_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<ReorderPlaylistRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let playlist_id = path.0.id;
    let owner: Option<(i64,)> =
        sqlx::query_as("SELECT owner_id FROM furumusic__playlist WHERE id = $1")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(owner) = owner else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    if owner.0 != user.id {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }

    let requested_ids = body
        .playlist_track_ids
        .into_iter()
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    let unique_requested = requested_ids.iter().copied().collect::<HashSet<_>>();
    if unique_requested.len() != requested_ids.len() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "duplicate playlist track ids",
        ));
    }

    let visible_ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT pt.id
           FROM furumusic__playlist_track pt
           JOIN furumusic__track t ON t.id = pt.track_id
           WHERE pt.playlist_id = $1 AND t.is_hidden = false
           ORDER BY pt.position"#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let visible_set = visible_ids.iter().copied().collect::<HashSet<_>>();
    if visible_set != unique_requested {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "playlist track ids do not match this playlist",
        ));
    }

    let hidden_ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT pt.id
           FROM furumusic__playlist_track pt
           JOIN furumusic__track t ON t.id = pt.track_id
           WHERE pt.playlist_id = $1 AND t.is_hidden = true
           ORDER BY pt.position"#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let mut ordered_ids = requested_ids;
    ordered_ids.extend(hidden_ids);
    if !ordered_ids.is_empty() {
        sqlx::query(
            r#"WITH ordered AS (
                 SELECT id, ord::integer - 1 AS position
                   FROM unnest($1::bigint[]) WITH ORDINALITY AS u(id, ord)
               )
               UPDATE furumusic__playlist_track pt
                  SET position = ordered.position
                 FROM ordered
                WHERE pt.id = ordered.id AND pt.playlist_id = $2"#,
        )
        .bind(&ordered_ids)
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    }

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query("UPDATE furumusic__playlist SET updated_at = $1 WHERE id = $2")
        .bind(&now)
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let reordered_track_ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT pt.track_id
           FROM furumusic__playlist_track pt
           JOIN furumusic__track t ON t.id = pt.track_id
           WHERE pt.playlist_id = $1 AND t.is_hidden = false
           ORDER BY pt.position"#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    record_fed_playlist_tracks_added(pool, user.id, playlist_id, &reordered_track_ids).await;

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// DELETE /api/player/playlists/{id}/tracks  — remove a track from playlist
// ---------------------------------------------------------------------------

async fn remove_track_from_playlist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
    Json(body): Json<RemoveTrackRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let playlist_id = path.0.id;
    let owner: Option<(i64,)> =
        sqlx::query_as("SELECT owner_id FROM furumusic__playlist WHERE id = $1")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(owner) = owner else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    if owner.0 != user.id {
        return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
    }
    let removed_content_ids = match crate::federation::devices::playlist_content_ids_for_removal(
        pool,
        playlist_id,
        body.playlist_track_id,
        body.track_id,
    )
    .await
    {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(
                playlist_id,
                "fed device playlist removal lookup failed: {err:#}"
            );
            Vec::new()
        }
    };

    match (body.playlist_track_id, body.track_id) {
        (Some(playlist_track_id), _) => {
            sqlx::query("DELETE FROM furumusic__playlist_track WHERE playlist_id = $1 AND id = $2")
                .bind(playlist_id)
                .bind(playlist_track_id)
                .execute(pool)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
        }
        (None, Some(track_id)) => {
            sqlx::query(
                "DELETE FROM furumusic__playlist_track WHERE playlist_id = $1 AND track_id = $2",
            )
            .bind(playlist_id)
            .bind(track_id)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        }
        (None, None) => {
            return Ok(json_error(StatusCode::BAD_REQUEST, "missing track id"));
        }
    }

    // Re-number positions
    sqlx::query(
        r#"WITH ordered AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY position) - 1 as new_pos
             FROM furumusic__playlist_track WHERE playlist_id = $1
           )
           UPDATE furumusic__playlist_track pt
           SET position = o.new_pos
           FROM ordered o WHERE pt.id = o.id"#,
    )
    .bind(playlist_id)
    .execute(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query("UPDATE furumusic__playlist SET updated_at = $1 WHERE id = $2")
        .bind(&now)
        .bind(playlist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    record_fed_playlist_tracks_removed(pool, user.id, playlist_id, &removed_content_ids).await;

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/player/likes/toggle/{track_id}  — toggle like on a track
// ---------------------------------------------------------------------------

async fn toggle_like_track_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathTrackId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let track_id = path.0.track_id;
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM furumusic__user_liked_track WHERE user_id = $1 AND track_id = $2",
    )
    .bind(user.id)
    .bind(track_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if existing.is_some() {
        sqlx::query("DELETE FROM furumusic__user_liked_track WHERE user_id = $1 AND track_id = $2")
            .bind(user.id)
            .bind(track_id)
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        record_fed_track_like(pool, user.id, track_id, false).await;
        Json(LikeStatus { liked: false }).into_response()
    } else {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        sqlx::query(
            "INSERT INTO furumusic__user_liked_track (user_id, track_id, created_at) VALUES ($1, $2, $3)",
        )
        .bind(user.id)
        .bind(track_id)
        .bind(&now)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        record_fed_track_like(pool, user.id, track_id, true).await;
        Json(LikeStatus { liked: true }).into_response()
    }
}

// ---------------------------------------------------------------------------
// POST /api/player/likes/release/{release_id}  — like all tracks in release
// ---------------------------------------------------------------------------

async fn like_release_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let release_id = path.0.id;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let track_ids = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM furumusic__track WHERE release_id = $1 AND is_hidden = false",
    )
    .bind(release_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    // Check if ALL tracks in this release are already liked
    let total = track_ids.len() as i64;

    let liked_count: (i64,) = sqlx::query_as(
        r#"SELECT COUNT(*) FROM furumusic__user_liked_track ult
           JOIN furumusic__track t ON t.id = ult.track_id
           WHERE ult.user_id = $1 AND t.release_id = $2 AND t.is_hidden = false"#,
    )
    .bind(user.id)
    .bind(release_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if liked_count.0 >= total && total > 0 {
        // Unlike all tracks in release
        sqlx::query(
            r#"DELETE FROM furumusic__user_liked_track
               WHERE user_id = $1 AND track_id IN (
                   SELECT id FROM furumusic__track WHERE release_id = $2 AND is_hidden = false
               )"#,
        )
        .bind(user.id)
        .bind(release_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        for track_id in &track_ids {
            record_fed_track_like(pool, user.id, *track_id, false).await;
        }
        Json(LikeStatus { liked: false }).into_response()
    } else {
        // Like all tracks in release (skip already liked)
        sqlx::query(
            r#"INSERT INTO furumusic__user_liked_track (user_id, track_id, created_at)
               SELECT $1, t.id, $3
               FROM furumusic__track t
               WHERE t.release_id = $2 AND t.is_hidden = false
                 AND NOT EXISTS (
                     SELECT 1 FROM furumusic__user_liked_track ult
                     WHERE ult.user_id = $1 AND ult.track_id = t.id
                 )"#,
        )
        .bind(user.id)
        .bind(release_id)
        .bind(&now)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        for track_id in &track_ids {
            record_fed_track_like(pool, user.id, *track_id, true).await;
        }
        Json(LikeStatus { liked: true }).into_response()
    }
}

// ---------------------------------------------------------------------------
// GET /api/player/likes  — get all liked track IDs for current user
// ---------------------------------------------------------------------------

async fn liked_ids_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT track_id FROM furumusic__user_liked_track WHERE user_id = $1")
            .bind(user.id)
            .fetch_all(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(LikedIds {
        track_ids: rows.into_iter().map(|r| r.0).collect(),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/player/follows  — get followed artists for current user
// ---------------------------------------------------------------------------

async fn followed_artists_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let rows = sqlx::query_as::<_, ArtistRow>(
        r#"SELECT a.id, a.name::text as name, a.image_file_id,
                  COALESCE(s.release_count, 0)::bigint AS release_count,
                  COALESCE(s.track_count, 0)::bigint AS track_count
           FROM furumusic__user_followed_artist ufa
           JOIN furumusic__artist a ON a.id = ufa.artist_id
           LEFT JOIN (
               SELECT ra.artist_id,
                      COUNT(DISTINCT r.id) AS release_count,
                      COUNT(t.id) AS track_count
               FROM furumusic__release_artist ra
               JOIN furumusic__release r ON r.id = ra.release_id AND r.is_hidden = false
               LEFT JOIN furumusic__track t ON t.release_id = r.id AND t.is_hidden = false
               WHERE ra.position = 0
               GROUP BY ra.artist_id
           ) s ON s.artist_id = a.id
           WHERE ufa.user_id = $1 AND a.is_hidden = false
           ORDER BY ufa.created_at DESC, a.name_sort"#,
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let artist_ids = rows.iter().map(|row| row.id).collect();
    let artists = rows
        .into_iter()
        .map(|r| ArtistCard {
            id: r.id,
            name: r.name,
            image_url: cover_variant_url(r.image_file_id, "small"),
            release_count: r.release_count,
            track_count: r.track_count,
        })
        .collect();

    Json(FollowedArtists {
        artist_ids,
        artists,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/player/follows/toggle/{id}  — follow/unfollow artist
// ---------------------------------------------------------------------------

async fn toggle_follow_artist_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathId>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let artist_id = path.0.id;

    let artist_exists: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM furumusic__artist WHERE id = $1 AND is_hidden = false")
            .bind(artist_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;

    if artist_exists.is_none() {
        return Ok(json_error(StatusCode::NOT_FOUND, "artist not found"));
    }

    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM furumusic__user_followed_artist WHERE user_id = $1 AND artist_id = $2",
    )
    .bind(user.id)
    .bind(artist_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if existing.is_some() {
        sqlx::query(
            "DELETE FROM furumusic__user_followed_artist WHERE user_id = $1 AND artist_id = $2",
        )
        .bind(user.id)
        .bind(artist_id)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        Json(FollowStatus { followed: false }).into_response()
    } else {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        sqlx::query(
            r#"INSERT INTO furumusic__user_followed_artist (user_id, artist_id, created_at)
               VALUES ($1, $2, $3)
               ON CONFLICT (user_id, artist_id) DO NOTHING"#,
        )
        .bind(user.id)
        .bind(artist_id)
        .bind(&now)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        Json(FollowStatus { followed: true }).into_response()
    }
}

// ---------------------------------------------------------------------------
// GET /api/player/radio/{kind}/{id}
// ---------------------------------------------------------------------------

fn append_unique_track_ids(track_ids: &mut Vec<i64>, candidates: Vec<i64>, limit: usize) {
    let mut seen: HashSet<i64> = track_ids.iter().copied().collect();
    for candidate in candidates {
        if track_ids.len() >= limit {
            break;
        }
        if seen.insert(candidate) {
            track_ids.push(candidate);
        }
    }
}

async fn track_primary_artist_ids(pool: &sqlx::PgPool, track_id: i64) -> cot::Result<Vec<i64>> {
    sqlx::query_scalar::<_, i64>(
        r#"SELECT artist_id
           FROM furumusic__track_artist
           WHERE track_id = $1 AND role <> 'featuring'
           ORDER BY position, artist_id"#,
    )
    .bind(track_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn release_primary_artist_ids(pool: &sqlx::PgPool, release_id: i64) -> cot::Result<Vec<i64>> {
    sqlx::query_scalar::<_, i64>(
        r#"SELECT artist_id
           FROM furumusic__release_artist
           WHERE release_id = $1
           ORDER BY position, artist_id"#,
    )
    .bind(release_id)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn fallback_radio_track_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    artist_ids: &[i64],
    excluded_ids: &[i64],
    limit: i64,
) -> cot::Result<Vec<i64>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    sqlx::query_scalar::<_, i64>(
        r#"SELECT t.id
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           WHERE t.is_hidden = false
             AND r.is_hidden = false
             AND NOT (t.id = ANY($3::bigint[]))
           ORDER BY (
               CASE WHEN EXISTS (
                   SELECT 1 FROM furumusic__user_liked_track ult
                   WHERE ult.user_id = $1 AND ult.track_id = t.id
               ) THEN 9.0 ELSE 0.0 END
               + CASE WHEN EXISTS (
                   SELECT 1 FROM furumusic__track_artist ta
                   WHERE ta.track_id = t.id
                     AND ta.role <> 'featuring'
                     AND ta.artist_id = ANY($2::bigint[])
               ) THEN 5.0 ELSE 0.0 END
               + COALESCE(t.lastfm_rating, 0.0) * 0.7
               + ln(COALESCE(t.lastfm_playcount, 0)::double precision + 1.0) * 0.04
               + ln(COALESCE(t.lastfm_listeners, 0)::double precision + 1.0) * 0.03
               + random() * 2.0
           ) DESC, t.id
           LIMIT $4"#,
    )
    .bind(user_id)
    .bind(artist_ids)
    .bind(excluded_ids)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn track_radio_candidate_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    track_id: i64,
    limit: i64,
) -> cot::Result<Vec<i64>> {
    sqlx::query_scalar::<_, i64>(
        r#"WITH seed_track AS (
               SELECT t.id, t.release_id
               FROM furumusic__track t
               JOIN furumusic__release r ON r.id = t.release_id
               WHERE t.id = $1 AND t.is_hidden = false AND r.is_hidden = false
           ),
           seed_artists AS (
               SELECT ta.artist_id
               FROM furumusic__track_artist ta
               JOIN seed_track st ON st.id = ta.track_id
               WHERE ta.role <> 'featuring'
           ),
           seed_tag_sources AS (
               SELECT egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__entity_genre_tag egt
               JOIN seed_track st ON egt.entity_kind = 'track' AND egt.entity_id = st.id
               UNION ALL
               SELECT tg.genre_id, 1.0 AS weight
               FROM furumusic__track_genre tg
               JOIN seed_track st ON st.id = tg.track_id
               UNION ALL
               SELECT egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__entity_genre_tag egt
               JOIN seed_track st ON egt.entity_kind = 'release' AND egt.entity_id = st.release_id
           ),
           seed_tags AS (
               SELECT genre_id, max(weight) AS seed_weight
               FROM seed_tag_sources
               GROUP BY genre_id
           ),
           candidate_tag_sources AS (
               SELECT egt.entity_id AS track_id,
                      egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__entity_genre_tag egt
               WHERE egt.entity_kind = 'track'
               UNION ALL
               SELECT tg.track_id, tg.genre_id, 1.0 AS weight
               FROM furumusic__track_genre tg
               UNION ALL
               SELECT t.id AS track_id,
                      egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__track t
               JOIN furumusic__entity_genre_tag egt
                 ON egt.entity_kind = 'release' AND egt.entity_id = t.release_id
           ),
           candidate_tags AS (
               SELECT track_id, genre_id, max(weight) AS weight
               FROM candidate_tag_sources
               GROUP BY track_id, genre_id
           ),
           tag_scores AS (
               SELECT ct.track_id, sum(st.seed_weight * ct.weight) AS tag_score
               FROM candidate_tags ct
               JOIN seed_tags st ON st.genre_id = ct.genre_id
               GROUP BY ct.track_id
           )
           SELECT t.id
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN tag_scores score ON score.track_id = t.id
           WHERE t.is_hidden = false
             AND r.is_hidden = false
             AND t.id <> $1
             AND (
                 COALESCE(score.tag_score, 0.0) > 0.0
                 OR EXISTS (
                     SELECT 1
                     FROM furumusic__track_artist ta
                     JOIN seed_artists sa ON sa.artist_id = ta.artist_id
                     WHERE ta.track_id = t.id AND ta.role <> 'featuring'
                 )
                 OR EXISTS (
                     SELECT 1 FROM furumusic__user_liked_track ult
                     WHERE ult.user_id = $2 AND ult.track_id = t.id
                 )
             )
           ORDER BY (
               COALESCE(score.tag_score, 0.0) * 12.0
               + CASE
                   WHEN EXISTS (
                       SELECT 1 FROM furumusic__user_liked_track ult
                       WHERE ult.user_id = $2 AND ult.track_id = t.id
                   ) AND COALESCE(score.tag_score, 0.0) > 0.0 THEN 12.0
                   WHEN EXISTS (
                       SELECT 1 FROM furumusic__user_liked_track ult
                       WHERE ult.user_id = $2 AND ult.track_id = t.id
                   ) THEN 3.0
                   ELSE 0.0
                 END
               + CASE WHEN EXISTS (
                     SELECT 1
                     FROM furumusic__track_artist ta
                     JOIN seed_artists sa ON sa.artist_id = ta.artist_id
                     WHERE ta.track_id = t.id AND ta.role <> 'featuring'
                 ) THEN 4.0 ELSE 0.0 END
               + COALESCE(t.lastfm_rating, 0.0) * 0.65
               + ln(COALESCE(t.lastfm_playcount, 0)::double precision + 1.0) * 0.04
               + ln(COALESCE(t.lastfm_listeners, 0)::double precision + 1.0) * 0.03
               + random() * 1.6
           ) DESC, t.id
           LIMIT $3"#,
    )
    .bind(track_id)
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn release_radio_candidate_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    release_id: i64,
    excluded_ids: &[i64],
    limit: i64,
) -> cot::Result<Vec<i64>> {
    sqlx::query_scalar::<_, i64>(
        r#"WITH seed_release AS (
               SELECT id
               FROM furumusic__release
               WHERE id = $1 AND is_hidden = false
           ),
           seed_artists AS (
               SELECT ra.artist_id
               FROM furumusic__release_artist ra
               JOIN seed_release sr ON sr.id = ra.release_id
           ),
           seed_tag_sources AS (
               SELECT egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__entity_genre_tag egt
               JOIN seed_release sr ON egt.entity_kind = 'release' AND egt.entity_id = sr.id
               UNION ALL
               SELECT egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__track t
               JOIN seed_release sr ON sr.id = t.release_id
               JOIN furumusic__entity_genre_tag egt
                 ON egt.entity_kind = 'track' AND egt.entity_id = t.id
               UNION ALL
               SELECT tg.genre_id, 1.0 AS weight
               FROM furumusic__track t
               JOIN seed_release sr ON sr.id = t.release_id
               JOIN furumusic__track_genre tg ON tg.track_id = t.id
           ),
           seed_tags AS (
               SELECT genre_id, max(weight) AS seed_weight
               FROM seed_tag_sources
               GROUP BY genre_id
           ),
           candidate_release_tag_sources AS (
               SELECT egt.entity_id AS release_id,
                      egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__entity_genre_tag egt
               WHERE egt.entity_kind = 'release'
               UNION ALL
               SELECT t.release_id,
                      egt.genre_id,
                      ln(greatest(COALESCE(egt.weight, 1.0), 1.0) + 1.0) AS weight
               FROM furumusic__track t
               JOIN furumusic__entity_genre_tag egt
                 ON egt.entity_kind = 'track' AND egt.entity_id = t.id
               UNION ALL
               SELECT t.release_id, tg.genre_id, 1.0 AS weight
               FROM furumusic__track t
               JOIN furumusic__track_genre tg ON tg.track_id = t.id
           ),
           candidate_release_tags AS (
               SELECT release_id, genre_id, max(weight) AS weight
               FROM candidate_release_tag_sources
               GROUP BY release_id, genre_id
           ),
           release_scores AS (
               SELECT crt.release_id, sum(st.seed_weight * crt.weight) AS tag_score
               FROM candidate_release_tags crt
               JOIN seed_tags st ON st.genre_id = crt.genre_id
               GROUP BY crt.release_id
           ),
           candidate_tracks AS (
               SELECT t.id,
                      t.release_id,
                      COALESCE(score.tag_score, 0.0) AS tag_score,
                      EXISTS (
                          SELECT 1 FROM furumusic__user_liked_track ult
                          WHERE ult.user_id = $2 AND ult.track_id = t.id
                      ) AS liked,
                      EXISTS (
                          SELECT 1
                          FROM furumusic__release_artist ra
                          JOIN seed_artists sa ON sa.artist_id = ra.artist_id
                          WHERE ra.release_id = t.release_id
                      ) AS same_artist,
                      COALESCE(t.lastfm_rating, 0.0) AS rating,
                      COALESCE(t.lastfm_playcount, 0)::double precision AS playcount,
                      COALESCE(t.lastfm_listeners, 0)::double precision AS listeners
               FROM furumusic__track t
               JOIN furumusic__release r ON r.id = t.release_id
               LEFT JOIN release_scores score ON score.release_id = t.release_id
               WHERE t.is_hidden = false
                 AND r.is_hidden = false
                 AND t.release_id <> $1
                 AND NOT (t.id = ANY($3::bigint[]))
                 AND (
                     COALESCE(score.tag_score, 0.0) > 0.0
                     OR EXISTS (
                         SELECT 1
                         FROM furumusic__release_artist ra
                         JOIN seed_artists sa ON sa.artist_id = ra.artist_id
                         WHERE ra.release_id = t.release_id
                     )
                     OR EXISTS (
                         SELECT 1 FROM furumusic__user_liked_track ult
                         WHERE ult.user_id = $2 AND ult.track_id = t.id
                     )
                 )
           ),
           ranked_tracks AS (
               SELECT *,
                      row_number() OVER (
                          PARTITION BY release_id
                          ORDER BY rating DESC, playcount DESC, listeners DESC, random() DESC, id
                      ) AS release_rank
               FROM candidate_tracks
           )
           SELECT id
           FROM ranked_tracks
           WHERE release_rank <= 4
           ORDER BY (
               tag_score * 12.0
               + CASE
                   WHEN liked AND tag_score > 0.0 THEN 11.0
                   WHEN liked THEN 3.0
                   ELSE 0.0
                 END
               + CASE WHEN same_artist THEN 3.5 ELSE 0.0 END
               + rating * 0.65
               + ln(playcount + 1.0) * 0.04
               + ln(listeners + 1.0) * 0.03
               + random() * 1.6
           ) DESC, id
           LIMIT $4"#,
    )
    .bind(release_id)
    .bind(user_id)
    .bind(excluded_ids)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))
}

async fn build_track_radio_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    track_id: i64,
) -> cot::Result<Option<Vec<i64>>> {
    let seed_track = sqlx::query_scalar::<_, i64>(
        r#"SELECT t.id
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           WHERE t.id = $1 AND t.is_hidden = false AND r.is_hidden = false"#,
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if seed_track.is_none() {
        return Ok(None);
    }

    let mut ids = vec![track_id];
    let candidate_ids =
        track_radio_candidate_ids(pool, user_id, track_id, PLAYER_RADIO_CANDIDATE_LIMIT).await?;
    append_unique_track_ids(&mut ids, candidate_ids, PLAYER_RADIO_TRACK_LIMIT);

    let artist_ids = track_primary_artist_ids(pool, track_id).await?;
    let remaining = PLAYER_RADIO_TRACK_LIMIT.saturating_sub(ids.len()) as i64;
    let fallback_ids =
        fallback_radio_track_ids(pool, user_id, &artist_ids, &ids, remaining).await?;
    append_unique_track_ids(&mut ids, fallback_ids, PLAYER_RADIO_TRACK_LIMIT);

    Ok(Some(ids))
}

async fn build_release_radio_ids(
    pool: &sqlx::PgPool,
    user_id: i64,
    release_id: i64,
) -> cot::Result<Option<Vec<i64>>> {
    let seed_release = sqlx::query_scalar::<_, i64>(
        r#"SELECT id FROM furumusic__release WHERE id = $1 AND is_hidden = false"#,
    )
    .bind(release_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    if seed_release.is_none() {
        return Ok(None);
    }

    let mut ids = sqlx::query_scalar::<_, i64>(
        r#"SELECT t.id
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           WHERE t.release_id = $1
             AND t.is_hidden = false
             AND r.is_hidden = false
           ORDER BY COALESCE(t.lastfm_rating, 0.0) DESC,
                    COALESCE(t.lastfm_playcount, 0) DESC,
                    COALESCE(t.lastfm_listeners, 0) DESC,
                    t.disc_number NULLS FIRST,
                    t.track_number NULLS LAST,
                    t.id
           LIMIT $2"#,
    )
    .bind(release_id)
    .bind(PLAYER_RADIO_RELEASE_SEED_TRACKS)
    .fetch_all(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let candidate_ids = release_radio_candidate_ids(
        pool,
        user_id,
        release_id,
        &ids,
        PLAYER_RADIO_CANDIDATE_LIMIT,
    )
    .await?;
    append_unique_track_ids(&mut ids, candidate_ids, PLAYER_RADIO_TRACK_LIMIT);

    let artist_ids = release_primary_artist_ids(pool, release_id).await?;
    let remaining = PLAYER_RADIO_TRACK_LIMIT.saturating_sub(ids.len()) as i64;
    let fallback_ids =
        fallback_radio_track_ids(pool, user_id, &artist_ids, &ids, remaining).await?;
    append_unique_track_ids(&mut ids, fallback_ids, PLAYER_RADIO_TRACK_LIMIT);

    Ok(Some(ids))
}

async fn radio_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathRadioSeed>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    let seed = path.0;
    let ids = match seed.kind.as_str() {
        "track" => build_track_radio_ids(pool, user.id, seed.id).await?,
        "release" => build_release_radio_ids(pool, user.id, seed.id).await?,
        _ => return Ok(json_error(StatusCode::BAD_REQUEST, "unknown radio seed")),
    };

    let Some(ids) = ids else {
        return Ok(json_error(StatusCode::NOT_FOUND, "radio seed not found"));
    };

    let tracks = load_track_items_by_ids(pool, &ids).await?;
    Json(tracks).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/player/tracks-by-ids
// ---------------------------------------------------------------------------

async fn tracks_by_ids_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    Json(body): Json<TracksByIdsRequest>,
) -> cot::Result<cot::response::Response> {
    let Some(_user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };

    if body.ids.is_empty() {
        return Json(Vec::<TrackItem>::new()).into_response();
    }

    let ids: Vec<i64> = body.ids.into_iter().take(500).collect();
    let result = load_track_items_by_ids(pool, &ids).await?;
    Json(result).into_response()
}

// ---------------------------------------------------------------------------
// PlayerApp
// ---------------------------------------------------------------------------

pub struct PlayerApp {
    config: Arc<AppConfig>,
    scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
    device_hub: Arc<PlayerDeviceHub>,
}

impl PlayerApp {
    pub fn new(
        config: Arc<AppConfig>,
        scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
    ) -> Self {
        Self {
            config,
            scheduler_handle,
            device_hub: PlayerDeviceHub::shared(),
        }
    }
}

impl App for PlayerApp {
    fn name(&self) -> &'static str {
        "player"
    }

    fn router(&self) -> Router {
        let pool_config = Arc::clone(&self.config);
        let pool: Arc<tokio::sync::OnceCell<sqlx::PgPool>> = Arc::new(tokio::sync::OnceCell::new());
        let torrent_service: Arc<tokio::sync::OnceCell<Arc<TorrentService>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let device_hub = Arc::clone(&self.device_hub);

        Router::with_urls([
            // -- Current user profile --
            Route::with_handler_and_name(
                "/me",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                me_handler(auth_ctx, session, db, pg_pool).await
                            }
                        },
                    )
                },
                "player_me",
            ),
            Route::with_handler_and_name(
                "/offline/manifest",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                offline_manifest_handler(auth_ctx, session, db, pg_pool).await
                            }
                        },
                    )
                },
                "player_offline_manifest",
            ),
            Route::with_handler_and_name(
                "/lastfm/status",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_status_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                }),
                "player_lastfm_status",
            ),
            Route::with_handler_and_name(
                "/lastfm/connect",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          request: cot::request::Request| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_connect_handler(auth_ctx, session, db, pg_pool, request).await
                        }
                    }
                }),
                "player_lastfm_connect",
            ),
            Route::with_handler_and_name(
                "/lastfm/callback",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: cot::request::extractors::UrlQuery<LastfmCallbackQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_callback_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    }
                }),
                "player_lastfm_callback",
            ),
            Route::with_handler_and_name(
                "/lastfm/disconnect",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_disconnect_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                }),
                "player_lastfm_disconnect",
            ),
            Route::with_handler_and_name(
                "/lastfm/now-playing",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<LastfmNowPlayingRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_now_playing_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_lastfm_now_playing",
            ),
            Route::with_handler_and_name(
                "/lastfm/scrobble",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<LastfmScrobbleRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            lastfm_scrobble_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_lastfm_scrobble",
            ),
            Route::with_handler_and_name(
                "/agent-queue",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                agent_queue_handler(auth_ctx, session, db, pg_pool).await
                            }
                        },
                    )
                },
                "player_agent_queue",
            ),
            // -- Torrent import widget --
            Route::with_handler_and_name(
                "/torrents",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    get(
                        move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.list(pg_pool, user.id).await {
                                    Ok(items) => Json(items).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::BAD_REQUEST, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_list",
            ),
            Route::with_handler_and_name(
                "/torrents/session/{id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    get({
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        let torrent_service = Arc::clone(&torrent_service);
                        let scheduler_handle = Arc::clone(&scheduler_handle);
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathStringId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.details(pg_pool, user.id, &path.0.id).await {
                                    Ok(details) => Json(details).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::NOT_FOUND, &err.to_string()))
                                    }
                                }
                            }
                        }
                    })
                    .delete(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathStringId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.remove(pg_pool, user.id, &path.0.id).await {
                                    Ok(()) => {
                                        Json(serde_json::json!({ "ok": true })).into_response()
                                    }
                                    Err(err) => {
                                        Ok(json_error(StatusCode::NOT_FOUND, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_detail",
            ),
            Route::with_handler_and_name(
                "/torrents/preview",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    post(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              json: Json<TorrentPreviewRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.preview(pg_pool, user.id, json.0).await {
                                    Ok(preview) => Json(preview).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::BAD_REQUEST, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_preview",
            ),
            Route::with_handler_and_name(
                "/uploads/local",
                {
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    post(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              request: cot::request::Request| {
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let (live_config, _) = AppConfig::load_with_db(&db).await;
                                local_upload_handler(
                                    auth_ctx,
                                    session,
                                    db,
                                    live_config,
                                    scheduler_handle,
                                    request,
                                )
                                .await
                            }
                        },
                    )
                },
                "player_local_upload",
            ),
            Route::with_handler_and_name(
                "/uploads/tracks",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<UserUploadsQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_uploads_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    }
                }),
                "player_upload_tracks",
            ),
            Route::with_handler_and_name(
                "/uploads/tracks/{track_id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathTrackId>,
                          json: Json<UserUploadTrackUpdateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_track_update_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                }),
                "player_upload_track_update",
            ),
            Route::with_handler_and_name(
                "/uploads/bulk-tracks",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<UserUploadBulkTrackUpdateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_tracks_bulk_update_handler(
                                auth_ctx, session, db, pg_pool, json,
                            )
                            .await
                        }
                    }
                }),
                "player_upload_tracks_bulk_update",
            ),
            Route::with_handler_and_name(
                "/uploads/releases/{id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<UserUploadReleaseUpdateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_release_update_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                }),
                "player_upload_release_update",
            ),
            Route::with_handler_and_name(
                "/uploads/reviews/{id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<UserUploadReviewUpdateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_review_save_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                }),
                "player_upload_review_save",
            ),
            Route::with_handler_and_name(
                "/uploads/reviews/{id}",
                delete({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_review_delete_handler(auth_ctx, session, db, pg_pool, path)
                                .await
                        }
                    }
                }),
                "player_upload_review_delete",
            ),
            Route::with_handler_and_name(
                "/uploads/reviews/{id}/approve",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<UserUploadReviewUpdateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            user_upload_review_approve_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                }),
                "player_upload_review_approve",
            ),
            Route::with_handler_and_name(
                "/torrents/{id}/start",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    post(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathStringId>,
                              json: Json<TorrentStartRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let (live_config, _) = AppConfig::load_with_db(&db).await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service
                                    .start(
                                        pg_pool,
                                        &path.0.id,
                                        json.0.selected_files,
                                        live_config.agent_inbox_dir,
                                        user.id,
                                    )
                                    .await
                                {
                                    Ok(job) => Json(job).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::BAD_REQUEST, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_start",
            ),
            Route::with_handler_and_name(
                "/torrents/{id}/pause",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    post(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathStringId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.pause(pg_pool, user.id, &path.0.id).await {
                                    Ok(job) => Json(job).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::BAD_REQUEST, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_pause",
            ),
            Route::with_handler_and_name(
                "/torrents/{id}/status",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let torrent_service = Arc::clone(&torrent_service);
                    let scheduler_handle = Arc::clone(&self.scheduler_handle);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathStringId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            let torrent_service = Arc::clone(&torrent_service);
                            let scheduler_handle = Arc::clone(&scheduler_handle);
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return Ok(json_error(
                                        StatusCode::UNAUTHORIZED,
                                        "not authenticated",
                                    ));
                                };
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let service = torrent_service
                                    .get_or_init(|| async {
                                        Arc::new(TorrentService::new(Arc::clone(&scheduler_handle)))
                                    })
                                    .await;
                                match service.status(pg_pool, user.id, &path.0.id).await {
                                    Ok(job) => Json(job).into_response(),
                                    Err(err) => {
                                        Ok(json_error(StatusCode::NOT_FOUND, &err.to_string()))
                                    }
                                }
                            }
                        },
                    )
                },
                "player_torrent_status",
            ),
            // -- Artists (paginated) --
            Route::with_handler_and_name(
                "/artists",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              query: cot::request::extractors::UrlQuery<PaginationQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            artists_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    })
                },
                "player_artists",
            ),
            // -- Artist detail --
            Route::with_handler_and_name(
                "/artists/{id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                artist_detail_handler(auth_ctx, session, db, pg_pool, path).await
                            }
                        },
                    )
                },
                "player_artist_detail",
            ),
            // -- Release detail --
            Route::with_handler_and_name(
                "/releases/{id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                release_detail_handler(auth_ctx, session, db, pg_pool, path).await
                            }
                        },
                    )
                },
                "player_release_detail",
            ),
            Route::with_handler_and_name(
                "/radio/{kind}/{id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathRadioSeed>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                radio_handler(auth_ctx, session, db, pg_pool, path).await
                            }
                        },
                    )
                },
                "player_radio",
            ),
            // -- Playlists (list + create) --
            Route::with_handler_and_name(
                "/playlists",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            playlists_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                })
                .post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<CreatePlaylistRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            create_playlist_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_playlists",
            ),
            // -- Shared playlist snapshots --
            Route::with_handler_and_name(
                "/share-playlist",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<CreatePlaylistShareRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            create_playlist_share_handler(auth_ctx, session, db, pg_pool, json)
                                .await
                        }
                    }
                }),
                "player_share_playlist",
            ),
            Route::with_handler_and_name(
                "/share-playlist/{id}",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathStringId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            playlist_share_detail_handler(auth_ctx, session, db, pg_pool, path)
                                .await
                        }
                    }
                }),
                "player_share_playlist_detail",
            ),
            // -- Playlist detail (get, update, delete) --
            Route::with_handler_and_name(
                "/playlists/{id}",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            playlist_detail_handler(auth_ctx, session, db, pg_pool, path).await
                        }
                    }
                })
                .put({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<UpdatePlaylistRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            update_playlist_handler(auth_ctx, session, db, pg_pool, path, json)
                                .await
                        }
                    }
                })
                .delete({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            delete_playlist_handler(auth_ctx, session, db, pg_pool, path).await
                        }
                    }
                }),
                "player_playlist_detail",
            ),
            // -- Playlist tracks (add / remove) --
            Route::with_handler_and_name(
                "/playlists/{id}/tracks",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<AddTracksRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            add_tracks_to_playlist_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                })
                .put({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<ReorderPlaylistRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            reorder_playlist_tracks_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                })
                .delete({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>,
                          json: Json<RemoveTrackRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            remove_track_from_playlist_handler(
                                auth_ctx, session, db, pg_pool, path, json,
                            )
                            .await
                        }
                    }
                }),
                "player_playlist_tracks",
            ),
            // -- Likes (get liked IDs) --
            Route::with_handler_and_name(
                "/likes",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            liked_ids_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                }),
                "player_likes",
            ),
            // -- Toggle like on track --
            Route::with_handler_and_name(
                "/likes/toggle/{track_id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathTrackId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            toggle_like_track_handler(auth_ctx, session, db, pg_pool, path).await
                        }
                    }
                }),
                "player_like_toggle",
            ),
            // -- Like/unlike release --
            Route::with_handler_and_name(
                "/likes/release/{id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            like_release_handler(auth_ctx, session, db, pg_pool, path).await
                        }
                    }
                }),
                "player_like_release",
            ),
            // -- Followed artists --
            Route::with_handler_and_name(
                "/follows",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            followed_artists_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                }),
                "player_follows",
            ),
            // -- Follow/unfollow artist --
            Route::with_handler_and_name(
                "/follows/toggle/{id}",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            toggle_follow_artist_handler(auth_ctx, session, db, pg_pool, path).await
                        }
                    }
                }),
                "player_follow_toggle",
            ),
            // -- Audio stream --
            Route::with_handler_and_name(
                "/stream/{track_id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathTrackId>,
                              request: cot::request::Request| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let (live_config, _) = AppConfig::load_with_db(&db).await;
                                stream_handler(
                                    auth_ctx,
                                    session,
                                    db,
                                    pg_pool,
                                    &live_config,
                                    &request,
                                    path,
                                )
                                .await
                            }
                        },
                    )
                },
                "player_stream",
            ),
            // -- Cover art --
            Route::with_handler_and_name(
                "/cover/{media_file_id}/{variant}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathMediaFileVariant>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let (live_config, _) = AppConfig::load_with_db(&db).await;
                                cover_variant_handler(
                                    auth_ctx,
                                    session,
                                    db,
                                    pg_pool,
                                    &live_config,
                                    path,
                                )
                                .await
                            }
                        },
                    )
                },
                "player_cover_variant",
            ),
            Route::with_handler_and_name(
                "/cover/{media_file_id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<PathMediaFileId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("player pool")
                                    })
                                    .await;
                                let (live_config, _) = AppConfig::load_with_db(&db).await;
                                cover_handler(auth_ctx, session, db, pg_pool, &live_config, path)
                                    .await
                            }
                        },
                    )
                },
                "player_cover",
            ),
            // -- Active browser devices --
            Route::with_handler_and_name(
                "/devices/heartbeat",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<DeviceHeartbeatRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            devices_heartbeat_handler(auth_ctx, session, db, device_hub, json).await
                        }
                    }
                }),
                "player_devices_heartbeat",
            ),
            Route::with_handler_and_name(
                "/devices/poll",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<DeviceHeartbeatRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            devices_poll_handler(auth_ctx, session, db, device_hub, json).await
                        }
                    }
                }),
                "player_devices_poll",
            ),
            Route::with_handler_and_name(
                "/devices/active",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<DeviceSelectRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            devices_select_handler(auth_ctx, session, db, device_hub, json).await
                        }
                    }
                }),
                "player_devices_active",
            ),
            Route::with_handler_and_name(
                "/devices/command",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<DeviceCommandRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            devices_command_handler(auth_ctx, session, db, device_hub, json).await
                        }
                    }
                }),
                "player_devices_command",
            ),
            // -- Federated TUI clients --
            Route::with_handler_and_name(
                "/fed-devices",
                get(
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| async move {
                        fed_devices_status_handler(auth_ctx, session, db).await
                    },
                ),
                "player_fed_devices",
            ),
            Route::with_handler_and_name(
                "/fed-devices/invite",
                post(
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| async move {
                        fed_devices_invite_handler(auth_ctx, session, db).await
                    },
                ),
                "player_fed_devices_invite",
            ),
            Route::with_handler_and_name(
                "/fed-devices/connect",
                post(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<FedDeviceConnectRequest>| async move {
                        fed_devices_connect_handler(auth_ctx, session, db, json).await
                    },
                ),
                "player_fed_devices_connect",
            ),
            Route::with_handler_and_name(
                "/fed-devices/pairing",
                post(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<FedDevicePairingAnswerRequest>| async move {
                        fed_devices_pairing_handler(auth_ctx, session, db, json).await
                    },
                ),
                "player_fed_devices_pairing",
            ),
            Route::with_handler_and_name(
                "/fed-devices/revoke",
                post(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<FedDeviceRevokeRequest>| async move {
                        fed_devices_revoke_handler(auth_ctx, session, db, json).await
                    },
                ),
                "player_fed_devices_revoke",
            ),
            Route::with_handler_and_name(
                "/fed-devices/sync",
                post(
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| async move {
                        fed_devices_sync_handler(auth_ctx, session, db).await
                    },
                ),
                "player_fed_devices_sync",
            ),
            Route::with_handler_and_name(
                "/jams/users",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<JamUserSearchQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            jam_users_search_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    }
                }),
                "player_jam_users",
            ),
            Route::with_handler_and_name(
                "/jams",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlayerJamCreateRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            jam_create_handler(auth_ctx, session, db, pg_pool, device_hub, json)
                                .await
                        }
                    }
                }),
                "player_jams_create",
            ),
            Route::with_handler_and_name(
                "/jams/join",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlayerJamJoinRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move { jam_join_handler(auth_ctx, session, db, device_hub, json).await }
                    }
                }),
                "player_jams_join",
            ),
            Route::with_handler_and_name(
                "/jams/invite",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlayerJamInviteRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        let device_hub = Arc::clone(&device_hub);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            jam_invite_handler(auth_ctx, session, db, pg_pool, device_hub, json)
                                .await
                        }
                    }
                }),
                "player_jams_invite",
            ),
            Route::with_handler_and_name(
                "/jams/leave",
                post({
                    let device_hub = Arc::clone(&device_hub);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlayerJamLeaveRequest>| {
                        let device_hub = Arc::clone(&device_hub);
                        async move { jam_leave_handler(auth_ctx, session, db, device_hub, json).await }
                    }
                }),
                "player_jams_leave",
            ),
            // -- Playback state GET --
            Route::with_handler_and_name(
                "/state",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            get_state_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                })
                .put({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlaybackStateDto>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            put_state_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                })
                .post({
                    // POST handler for sendBeacon (used on page unload)
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PlaybackStateDto>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            put_state_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_state",
            ),
            // -- Play history --
            Route::with_handler_and_name(
                "/history",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: cot::request::extractors::UrlQuery<HistoryQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            history_list_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    }
                })
                .post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<HistoryEntry>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            history_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_history",
            ),
            // -- Search --
            Route::with_handler_and_name(
                "/search",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: cot::request::extractors::UrlQuery<SearchQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            search_handler(auth_ctx, session, db, pg_pool, query).await
                        }
                    }
                }),
                "player_search",
            ),
            Route::with_handler_and_name(
                "/federation/search/events",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: cot::request::extractors::UrlQuery<SearchQuery>| async move {
                        federation_search_events_handler(auth_ctx, session, db, query).await
                    },
                ),
                "player_federation_search_events",
            ),
            Route::with_handler_and_name(
                "/tracks/content/like",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext, session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            content_likes_handler(auth_ctx, session, db, pg_pool).await
                        }
                    }
                })
                .post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<ContentTrackMutation>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            content_like_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_content_like",
            ),
            Route::with_handler_and_name(
                "/tracks/content/playlist",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<ContentTrackMutation>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            content_playlist_add_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_content_playlist",
            ),
            Route::with_handler_and_name(
                "/federation/tracks/prepare",
                post(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<PrepareFederatedTrackRequest>| async move {
                        prepare_federated_track_handler(auth_ctx, session, db, json).await
                    },
                ),
                "player_federation_track_prepare",
            ),
            Route::with_handler_and_name(
                "/playlists/{id}/federation-tracks",
                get({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            federation_playlist_tracks_handler(
                                auth_ctx, session, db, pg_pool, path,
                            )
                            .await
                        }
                    }
                }),
                "player_federation_playlist_tracks",
            ),
            Route::with_handler_and_name(
                "/federation/artists/events",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<FederationArtistQuery>| async move {
                        federation_artist_events_handler(auth_ctx, session, db, query).await
                    },
                ),
                "player_federation_artist_events",
            ),
            Route::with_handler_and_name(
                "/federation/tracks/artwork",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<FederationArtworkQuery>| async move {
                        federation_track_artwork_handler(auth_ctx, session, db, query).await
                    },
                ),
                "player_federation_track_artwork",
            ),
            Route::with_handler_and_name(
                "/federation/catalog/artwork",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<FederationCatalogArtworkQuery>| async move {
                        federation_catalog_artwork_handler(auth_ctx, session, db, query).await
                    },
                ),
                "player_federation_catalog_artwork",
            ),
            Route::with_handler_and_name(
                "/federation/catalog/artwork/discover",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          query: UrlQuery<FederationArtworkDiscoveryQuery>| async move {
                        federation_artwork_discovery_handler(auth_ctx, session, db, query).await
                    },
                ),
                "player_federation_artwork_discovery",
            ),
            Route::with_handler_and_name(
                "/federation/cache/{id}",
                get(
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          request: cot::request::Request,
                          path: Path<PathStringId>| async move {
                        federation_cache_stream_handler(auth_ctx, session, db, request, path).await
                    },
                ),
                "player_federation_cache_stream",
            ),
            // -- Tracks by IDs --
            Route::with_handler_and_name(
                "/tracks-by-ids",
                post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |auth_ctx: auth::AuthContext,
                          session: Session,
                          db: Database,
                          json: Json<TracksByIdsRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("player pool")
                                })
                                .await;
                            tracks_by_ids_handler(auth_ctx, session, db, pg_pool, json).await
                        }
                    }
                }),
                "player_tracks_by_ids",
            ),
        ])
    }
}
