// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 3 and § 9.2: a game runtime build enables `dcc-feed` without `dcc-feed-assets`. It
//! still applies transform, camera, light, skeleton and generic items, and answers
//! `kind_not_supported` for mesh declarations, texture pushes and clip pushes. Runs only in that
//! build: `cargo test -p regia_agent --features dcc-feed --test dcc_feed_runtime_kinds`.
#![cfg(all(feature = "dcc-feed", not(feature = "dcc-feed-assets")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use regia_agent::dcc_feed::core::{ASSET_KINDS, FeedCore};
use regia_agent::dcc_feed::net::lock;
use regia_agent::dcc_feed::session::{SessionId, Transport};
use regia_agent::dcc_feed::wire::{
    ClipPush, FeedKind, FrameInfo, Hello, ItemDeclare, Message, TexturePush, TextureSource, TransformBlock,
    TransformFrame,
};
use regia_agent::dcc_feed::{
    DccFeedClock, DccFeedPlugin, DccFeedRegistry, DccFeedServer, DccFeedSettings, FeedItem, item_guid,
};
use regia_app::App;

fn declare(handle: u32, key: &str, kind: FeedKind) -> Message {
    Message::ItemDeclare(ItemDeclare {
        handle,
        key: key.to_owned(),
        kind,
        parent_handle: 0,
        display_name: key.to_owned(),
        variant: 0,
        channels: Vec::new(),
    })
}

fn errors(core: &Mutex<FeedCore>, s: SessionId) -> Vec<(u32, String)> {
    lock(core)
        .take_outbox(s)
        .iter()
        .filter_map(|m| match m {
            Message::Error(e) => Some((e.handle, e.code.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_runtime_build_refuses_asset_kinds_and_applies_the_rest() {
    const { assert!(!ASSET_KINDS, "this test is for the runtime build") };
    let settings = DccFeedSettings {
        auto_accept: vec!["tool".to_owned()],
        ..DccFeedSettings::default()
    };
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(DccFeedPlugin { project_dir: None });
    let server = DccFeedServer::new(&settings, None);
    let core = server.core().clone();
    app.insert_resource(settings);
    app.insert_resource(server);
    app.insert_resource(DccFeedClock {
        fixed_step_s: Some(1.0 / 60.0),
        now_s: 0.0,
    });

    let s = {
        let mut c = lock(&core);
        let s = c.open(Transport::Unix, "unix", None);
        c.submit(
            s,
            1,
            40,
            Message::Hello(Hello {
                min_version: 1,
                max_version: 1,
                client_name: "tool".to_owned(),
                client_version: "1".to_owned(),
                dcc: "test".to_owned(),
                space: "regia".to_owned(),
                unit_scale: 1.0,
                caps: 0,
                token: String::new(),
            }),
        );
        s
    };
    app.update();
    lock(&core).take_outbox(s);

    let mut seq = 2;
    let mut send = |msg: Message| {
        lock(&core).submit(s, seq, 64, msg);
        seq += 1;
    };
    send(declare(1, "Box", FeedKind::Transform));
    send(declare(2, "Hull", FeedKind::Mesh));
    send(Message::Transform(TransformFrame {
        handle: 1,
        info: FrameInfo {
            frame_id: 1,
            source_time_ns: 0,
            timecode: None,
        },
        xf: TransformBlock {
            translation: [2.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            world_space: false,
        },
    }));
    send(Message::TexturePush(TexturePush {
        name: "white".to_owned(),
        colorspace: 0,
        source: TextureSource::Raw {
            pixel_format: 0,
            width: 1,
            height: 1,
            bytes: vec![0xff; 4],
        },
    }));
    send(Message::ClipPush(ClipPush {
        name: "wave".to_owned(),
        fps: 30.0,
        duration_s: 1.0,
        looping: false,
        bind_handle: 0,
        tracks: Vec::new(),
    }));
    app.update();

    let mut codes = errors(&core, s);
    codes.sort();
    assert_eq!(
        codes,
        vec![
            (0, "kind_not_supported".to_owned()),
            (0, "kind_not_supported".to_owned()),
            (2, "kind_not_supported".to_owned()),
        ],
        "the mesh declaration and both pushes are refused, nothing else"
    );
    let registry = app.world().resource::<DccFeedRegistry>();
    assert!(registry.table.get(item_guid("Hull")).is_none(), "no mesh item");
    let entity = registry
        .table
        .get(item_guid("Box"))
        .and_then(|i| i.entity)
        .expect("the transform item applies");
    let item = app.world().get::<FeedItem>(entity).expect("FeedItem");
    assert_eq!(item.frames_applied, 1);
}
