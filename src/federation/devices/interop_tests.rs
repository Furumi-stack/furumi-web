//! Cross-binary protocol contract; the peer is the TUI's production adapter.
use super::*;
use tokio::io::AsyncWriteExt;

#[tokio::test]
#[ignore = "run furumi_tui/scripts/test_device_interop.py"]
async fn localhost_tui_peer() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir =
            std::path::PathBuf::from(std::env::var("FURUMI_INTEROP_DIR").expect("interop runner"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(
            dir.join("web-address"),
            listener.local_addr().unwrap().to_string(),
        )
        .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hub = PlayerDeviceHub::default();
        let web_id = "web-interop";
        let mut engine = Engine::new(
            web_id.into(),
            PlaybackConfig::passive(),
            Default::default(),
            0,
        );
        for phase in 0..3 {
            let hello: WireMessage =
                serde_json::from_slice(&read_line(&mut stream).await.unwrap()).unwrap();
            let WireMessage::Hello {
                profile,
                playback: Some(snapshot),
                ..
            } = hello
            else {
                panic!("expected TUI hello")
            };
            assert_eq!(profile.protocol_version, PROTOCOL_VERSION);
            assert_eq!(snapshot.device_id, profile.device_id);
            let announcement = snapshot
                .coordination
                .as_ref()
                .expect("versioned playback envelope");
            assert!(engine.observe(&profile.device_id, announcement, phase * 1000));
            if phase == 0 {
                assert_eq!(engine.owner(), Some(profile.device_id.as_str()));
                assert!(
                    !engine.tick(10_000),
                    "passive web server must not seize playback"
                );
                assert_eq!(snapshot.state.position_secs, 42.5);
                assert_eq!(snapshot.state.volume, 73);
                assert!(snapshot.state.shuffle);
                // Exercise the production browser hub projection without a
                // PostgreSQL library or an audio device.
                hub.apply_fed_playback_state_json(
                    1,
                    &profile.device_id,
                    &profile.name,
                    true,
                    serde_json::json!({"tracks": [], "index": 0, "track": null,
                        "position_seconds": 42.5, "duration_seconds": 100.0,
                        "paused": false, "shuffle": true, "repeat_mode": "all",
                        "volume": 0.73, "updated_at_ms": now_ms()}),
                )
                .unwrap();
                assert_eq!(
                    hub.active_device_id_for_commands(1),
                    Some(format!("fed:{}", profile.device_id))
                );
            } else {
                assert_eq!(
                    engine.owner(),
                    Some(if phase == 1 {
                        web_id
                    } else {
                        profile.device_id.as_str()
                    })
                );
            }
            let mut state = snapshot.state;
            let command = if phase < 2 {
                let owner = if phase == 0 {
                    web_id
                } else {
                    profile.device_id.as_str()
                };
                assert!(engine.transfer(owner, (phase + 1) * 1000));
                PlaybackCommand::ActiveChanged {
                    active_device_id: owner.into(),
                    active_device_name: "interop".into(),
                    state: state.clone(),
                }
            } else {
                state.paused = true;
                state.position_secs = 87.0;
                PlaybackCommand::SetState {
                    state: state.clone(),
                    seek: true,
                }
            };
            engine.set_output(true, engine.is_owner());
            engine.heartbeat((phase + 1) * 1000);
            let response = WireMessage::SyncResponse {
                accepted: true,
                error: None,
                devices: vec![],
                vector: BTreeMap::new(),
                snapshot: SyncSnapshot::default(),
                playback: Some(PlaybackSnapshot {
                    device_id: web_id.into(),
                    device_name: "WEB".into(),
                    active: engine.is_owner(),
                    updated_at_ms: now_ms(),
                    state,
                    coordination: Some(engine.announcement()),
                }),
                ops: vec![SyncOpWire {
                    op_id: format!("{web_id}:{}", phase + 1),
                    origin_device_id: web_id.into(),
                    seq: (phase + 1) as i64,
                    hlc_ms: now_ms(),
                    payload: SyncOpPayload::PlaybackCommand {
                        target_device_id: profile.device_id,
                        command,
                        authority: engine.stamp(),
                    },
                }],
            };
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).await.unwrap();
        }
        assert_eq!(read_line(&mut stream).await.unwrap(), b"ok");
    })
    .await
    .expect("TUI/web exchange timed out");
}

fn projected_snapshot(
    owner: &mut Engine,
    title: &str,
    now: u64,
) -> (PlaybackSnapshot, serde_json::Value) {
    owner.heartbeat(now);
    let snapshot = PlaybackSnapshot {
        device_id: "tui".into(),
        device_name: "TUI".into(),
        active: true,
        updated_at_ms: now as i64,
        state: serde_json::from_value(serde_json::json!({
            "playing": true, "paused": false, "position_secs": 0.0,
            "shuffle": false, "repeat": "off"
        }))
        .unwrap(),
        coordination: Some(owner.announcement()),
    };
    let track = serde_json::json!({"id": now, "title": title});
    let payload = serde_json::json!({
        "track": track, "tracks": [track], "index": 0,
        "position_seconds": 0.0, "duration_seconds": 200.0,
        "paused": false, "shuffle": false, "repeat_mode": "off", "volume": 0.74
    });
    (snapshot, payload)
}

fn projection_engines() -> (Engine, Engine) {
    let mut tui = Engine::new(
        "tui".into(),
        PlaybackConfig::default(),
        Default::default(),
        0,
    );
    tui.set_output(true, true);
    assert!(tui.transfer("tui", 0));
    let web = Engine::new(
        "web".into(),
        PlaybackConfig::passive(),
        Default::default(),
        0,
    );
    (tui, web)
}

#[test]
fn control_projection_advances_while_newer_snapshots_are_still_materializing() {
    let (mut tui, mut web) = projection_engines();
    let hub = PlayerDeviceHub::default();
    let (initial, initial_payload) = projected_snapshot(&mut tui, "Gabriela", 1);
    assert!(web.observe("tui", initial.coordination.as_ref().unwrap(), 1));
    project_ready_playback_snapshot(&web, &hub, 1, &initial, initial_payload).unwrap();

    // Every slow catalog conversion overlaps the arrival of the next poll.
    // Arrival must not call observe: ownership/liveness and hub projection
    // advance together only once that snapshot has finished materializing.
    let mut ready = projected_snapshot(&mut tui, "Track 2", 2);
    for now in 3..8 {
        let pending = projected_snapshot(&mut tui, &format!("Track {now}"), now);
        assert!(web.observe("tui", ready.0.coordination.as_ref().unwrap(), now));
        project_ready_playback_snapshot(&web, &hub, 1, &ready.0, ready.1).unwrap();
        let state = hub.playback_state_json_for_commands(1).unwrap();
        assert_eq!(state["track"]["title"], format!("Track {}", now - 1));
        assert_eq!(state["tracks"][0]["title"], state["track"]["title"]);
        assert_eq!(
            hub.active_device_id_for_commands(1).as_deref(),
            Some("fed:tui")
        );
        ready = pending;
    }
}

#[test]
fn delayed_projection_cannot_replace_a_newer_track_or_owner() {
    let (mut tui, mut web) = projection_engines();
    let hub = PlayerDeviceHub::default();
    let (old, old_payload) = projected_snapshot(&mut tui, "Old", 1);
    let (new, new_payload) = projected_snapshot(&mut tui, "Current", 2);
    assert!(web.observe("tui", new.coordination.as_ref().unwrap(), 2));
    project_ready_playback_snapshot(&web, &hub, 1, &new, new_payload).unwrap();
    assert!(web.observe("tui", old.coordination.as_ref().unwrap(), 3));
    project_ready_playback_snapshot(&web, &hub, 1, &old, old_payload.clone()).unwrap();
    assert_eq!(
        hub.playback_state_json_for_commands(1).unwrap()["track"]["title"],
        "Current"
    );

    // A user can change ownership during catalog preparation. Recheck after
    // preparation and keep the previous projection instead of applying it.
    assert!(web.transfer("another-output", 4));
    assert!(web.observe("tui", new.coordination.as_ref().unwrap(), 5));
    project_ready_playback_snapshot(&web, &hub, 1, &new, old_payload).unwrap();
    assert_eq!(
        hub.playback_state_json_for_commands(1).unwrap()["track"]["title"],
        "Current"
    );
    assert_eq!(web.owner(), Some("another-output"));
}
