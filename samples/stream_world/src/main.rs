// SPDX-License-Identifier: MIT-0
//! `stream_world --generate <file>` writes the level; `--generate-bakeable <dir> <cells_x> <cells_z> <props>`
//! writes a project of static glTF props for `regia bake-lightmaps --scene`; `stream_world --project <dir>` flies a cooked
//! project (see the crate docs).

use regia::prelude::*;
use regia_scene::Transform;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::streaming::{SourceKind, StreamingSource};
use regia_world::{WorldOrigin, open_streamed_level};
use stream_world::{Flight, FlightStep, level_text, write_bakeable_project};

/// The level to open, from the `.meta` the cook wrote.
#[derive(Resource)]
struct StartLevel(LevelId);

/// The flight's progress.
#[derive(Resource, Default)]
struct Progress {
    step: u32,
}

#[derive(Component)]
struct Flyer;

fn open(mut commands: Commands, level: Res<StartLevel>) {
    let level = level.0;
    commands.queue(move |world: &mut World| {
        if let Err(error) = open_streamed_level(world, level, LoadMode::Replace) {
            eprintln!("stream_world: {error}");
        }
    });
    commands.spawn((
        Flyer,
        Transform::from_translation(regia_core::math::Vec3::new(
            Flight::START[0] as f32,
            Flight::START[1] as f32,
            Flight::START[2] as f32,
        )),
        StreamingSource {
            kind: SourceKind::Camera,
            drives_origin: true,
            ..StreamingSource::default()
        },
        regia_render::Camera::default(),
    ));
}

fn fly(
    mut progress: ResMut<Progress>,
    origin: Res<WorldOrigin>,
    mut flyer: Query<&mut Transform, With<Flyer>>,
    mut exit: MessageWriter<AppExit>,
) {
    let position = match Flight.step(progress.step) {
        FlightStep::At(position) | FlightStep::Teleport(position) => position,
        FlightStep::End => {
            exit.write(AppExit::Success);
            return;
        }
    };
    progress.step += 1;
    if let (Ok(local), Ok(mut transform)) = (origin.from_world(position), flyer.single_mut()) {
        transform.translation = regia_core::math::Vec3::from_array(local);
    }
}

/// The flight as a plugin.
struct StreamWorldSample(LevelId);

impl Plugin for StreamWorldSample {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .insert_resource(StartLevel(self.0))
            .init_resource::<Progress>()
            .add_systems(Startup, open)
            .add_systems(FixedUpdate, fly.before(regia_world::StreamSet::Sources));
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: stream_world --generate <file> | --generate-bakeable <dir> <cells_x> <cells_z> <props> | --project <dir> --level <guid>"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--generate", file] => {
            if let Err(error) = std::fs::write(file, level_text(0x5eed)) {
                eprintln!("stream_world: {file}: {error}");
                std::process::exit(1);
            }
        }
        ["--generate-bakeable", dir, cells_x, cells_z, props] => {
            let (Ok(x), Ok(z), Ok(p)) = (cells_x.parse(), cells_z.parse(), props.parse()) else {
                usage()
            };
            if let Err(error) = write_bakeable_project(std::path::Path::new(dir), 0x5eed, x, z, p) {
                eprintln!("stream_world: {dir}: {error}");
                std::process::exit(1);
            }
        }
        ["--project", dir, "--level", guid] => {
            let Ok(guid) = guid.parse() else { usage() };
            std::env::set_current_dir(dir).unwrap_or_else(|error| {
                eprintln!("stream_world: {dir}: {error}");
                std::process::exit(1);
            });
            regia::run(StreamWorldSample(LevelId(guid)));
        }
        _ => usage(),
    }
}
