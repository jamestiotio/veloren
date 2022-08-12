pub mod event;
pub mod rule;
pub mod tick;

use common::{
    grid::Grid,
    slowjob::SlowJobPool,
    rtsim::{ChunkResource, RtSimEntity},
    terrain::{TerrainChunk, Block},
    vol::RectRasterableVol,
};
use common_ecs::{dispatch, System};
use rtsim2::{
    data::{
        npc::NpcMode,
        Data,
        ReadError,
    },
    rule::Rule,
    event::OnSetup,
    RtState,
};
use specs::{DispatcherBuilder, WorldExt};
use std::{
    fs::{self, File},
    path::PathBuf,
    sync::Arc,
    time::Instant,
    io::{self, Write},
    error::Error,
};
use enum_map::EnumMap;
use tracing::{error, warn, info, debug};
use vek::*;
use world::{IndexRef, World};

pub struct RtSim {
    file_path: PathBuf,
    last_saved: Option<Instant>,
    state: RtState,
}

impl RtSim {
    pub fn new(index: IndexRef, world: &World, data_dir: PathBuf) -> Result<Self, ron::Error> {
        let file_path = Self::get_file_path(data_dir);

        info!("Looking for rtsim data in {}...", file_path.display());
        let data = 'load: {
            if std::env::var("RTSIM_NOLOAD").map_or(true, |v| v != "1") {
                match File::open(&file_path) {
                    Ok(file) => {
                        info!("Rtsim data found. Attempting to load...");
                        match Data::from_reader(io::BufReader::new(file)) {
                            Ok(data) => { info!("Rtsim data loaded."); break 'load data },
                            Err(e) => {
                                error!("Rtsim data failed to load: {}", e);
                                let mut i = 0;
                                loop {
                                    let mut backup_path = file_path.clone();
                                    backup_path.set_extension(if i == 0 {
                                        format!("backup_{}", i)
                                    } else {
                                        "ron_backup".to_string()
                                    });
                                    if !backup_path.exists() {
                                        fs::rename(&file_path, &backup_path)?;
                                        warn!("Failed rtsim data was moved to {}", backup_path.display());
                                        info!("A fresh rtsim data will now be generated.");
                                        break;
                                    }
                                    i += 1;
                                }
                            },
                        }
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound =>
                        info!("No rtsim data found. Generating from world..."),
                    Err(e) => return Err(e.into()),
                }
            } else {
                warn!("'RTSIM_NOLOAD' is set, skipping loading of rtsim state (old state will be overwritten).");
            }

            let data = Data::generate(&world, index);
            info!("Rtsim data generated.");
            data
        };

        let mut this = Self {
            last_saved: None,
            state: RtState::new(data)
                .with_resource(ChunkStates(Grid::populate_from(world.sim().get_size().as_(), |_| None))),
            file_path,
        };

        rule::start_rules(&mut this.state);

        this.state.emit(OnSetup, world, index);

        Ok(this)
    }

    fn get_file_path(mut data_dir: PathBuf) -> PathBuf {
        let mut path = std::env::var("VELOREN_RTSIM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                data_dir.push("rtsim");
                data_dir
            });
        path.push("data.dat");
        path
    }

    pub fn hook_load_chunk(&mut self, key: Vec2<i32>, max_res: EnumMap<ChunkResource, usize>) {
        if let Some(chunk_state) = self.state.resource_mut::<ChunkStates>().0.get_mut(key) {
            *chunk_state = Some(LoadedChunkState { max_res });
        }
    }

    pub fn hook_unload_chunk(&mut self, key: Vec2<i32>) {
        if let Some(chunk_state) = self.state.resource_mut::<ChunkStates>().0.get_mut(key) {
            *chunk_state = None;
        }
    }

    pub fn hook_block_update(&mut self, world: &World, index: IndexRef, wpos: Vec3<i32>, old: Block, new: Block) {
        self.state.emit(event::OnBlockChange { wpos, old, new }, world, index);
    }

    pub fn hook_rtsim_entity_unload(&mut self, entity: RtSimEntity) {
        if let Some(npc) = self.state.data_mut().npcs.get_mut(entity.0) {
            npc.mode = NpcMode::Simulated;
        }
    }

    pub fn hook_rtsim_entity_delete(&mut self, entity: RtSimEntity) {
        // TODO: Emit event on deletion to catch death?
        self.state.data_mut().npcs.remove(entity.0);
    }

    pub fn save(&mut self, slowjob_pool: &SlowJobPool) {
        info!("Saving rtsim data...");
        let file_path = self.file_path.clone();
        let data = self.state.data().clone();
        debug!("Starting rtsim data save job...");
        // TODO: Use slow job
        // slowjob_pool.spawn("RTSIM_SAVE", move || {
        std::thread::spawn(move || {
            let tmp_file_name = "data_tmp.dat";
            if let Err(e) = file_path
                .parent()
                .map(|dir| {
                    fs::create_dir_all(dir)?;
                    // We write to a temporary file and then rename to avoid corruption.
                    Ok(dir.join(tmp_file_name))
                })
                .unwrap_or_else(|| Ok(tmp_file_name.into()))
                .and_then(|tmp_file_path| {
                    Ok((File::create(&tmp_file_path)?, tmp_file_path))
                })
                .map_err(|e: io::Error| Box::new(e) as Box::<dyn Error>)
                .and_then(|(mut file, tmp_file_path)| {
                    debug!("Writing rtsim data to file...");
                    data.write_to(io::BufWriter::new(&mut file))?;
                    file.flush()?;
                    drop(file);
                    fs::rename(tmp_file_path, file_path)?;
                    debug!("Rtsim data saved.");
                    Ok(())
                })
            {
                error!("Saving rtsim data failed: {}", e);
            }
        });
        self.last_saved = Some(Instant::now());
    }

    // TODO: Clean up this API a bit
    pub fn get_chunk_resources(&self, key: Vec2<i32>) -> EnumMap<ChunkResource, f32> {
        self.state.data().nature.get_chunk_resources(key)
    }

    pub fn state(&self) -> &RtState {
        &self.state
    }
}

struct ChunkStates(pub Grid<Option<LoadedChunkState>>);

struct LoadedChunkState {
    // The maximum possible number of each resource in this chunk
    max_res: EnumMap<ChunkResource, usize>,
}

pub fn add_server_systems(dispatch_builder: &mut DispatcherBuilder) {
    dispatch::<tick::Sys>(dispatch_builder, &[]);
}
