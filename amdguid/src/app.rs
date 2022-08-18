use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use amdgpu::pidfile::ports::{Output, OutputType};
use amdgpu::pidfile::Pid;
use egui::Ui;
use epaint::ColorImage;
use epi::Frame;
use image::{GenericImageView, ImageBuffer, ImageFormat};
use parking_lot::Mutex;

use crate::widgets::outputs_settings::OutputsSettings;
use crate::widgets::{ChangeFanSettings, CoolingPerformance};

pub enum ChangeState {
    New,
    Reloading,
    Success,
    Failure(String),
}

impl Default for ChangeState {
    fn default() -> Self {
        ChangeState::New
    }
}

pub struct FanService {
    pub pid: Pid,
    pub reload: ChangeState,
}

impl FanService {
    pub fn new(pid: Pid) -> FanService {
        Self {
            pid,
            reload: Default::default(),
        }
    }
}

pub struct FanServices(pub Vec<FanService>);

impl FanServices {
    pub fn list_changed(&self, other: &[Pid]) -> bool {
        if self.0.len() != other.len() {
            return true;
        }
        let c = self
            .0
            .iter()
            .fold(HashMap::with_capacity(other.len()), |mut h, service| {
                h.insert(service.pid.0, true);
                h
            });
        !other.iter().all(|s| c.contains_key(&s.0))
    }
}

impl From<Vec<Pid>> for FanServices {
    fn from(v: Vec<Pid>) -> Self {
        Self(v.into_iter().map(FanService::new).collect())
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Page {
    Config,
    Monitoring,
    Outputs,
    Settings,
}

impl Default for Page {
    fn default() -> Self {
        Self::Config
    }
}

pub type FanConfig = Arc<Mutex<amdgpu_config::fan::Config>>;

#[cfg(not(debug_assertions))]
static RELOAD_PID_LIST_DELAY: u8 = 18;
#[cfg(debug_assertions)]
static RELOAD_PID_LIST_DELAY: u8 = 80;

pub struct StatefulConfig {
    pub config: FanConfig,
    pub state: ChangeState,
    pub textures: HashMap<OutputType, epaint::TextureHandle>,
}

impl StatefulConfig {
    pub fn new(config: FanConfig) -> Self {
        let textures = HashMap::with_capacity(40);

        Self {
            config,
            state: ChangeState::New,
            textures,
        }
    }

    pub fn load_textures(&mut self, ui: &mut Ui) {
        if !self.textures.is_empty() {
            return;
        }

        // 80x80
        let image = {
            let bytes = include_bytes!("../assets/icons/ports2.jpg");
            image::load_from_memory_with_format(bytes, ImageFormat::Jpeg).unwrap()
        };

        let ctx = ui.ctx();

        for ty in OutputType::all() {
            let (offset_x, offset_y) = ty.to_coords();
            let mut img = ImageBuffer::new(80, 80);
            for x in 0..80 {
                for y in 0..80 {
                    img.put_pixel(x, y, image.get_pixel(x + offset_x, y + offset_y));
                }
            }

            let size = [img.width() as _, img.height() as _];
            let pixels = img.as_flat_samples();
            let id = ctx.load_texture(
                String::from(ty.name()),
                epaint::ImageData::Color(ColorImage::from_rgba_unmultiplied(
                    size,
                    pixels.as_slice(),
                )),
            );
            self.textures.insert(ty, id);
        }
    }
}

pub struct AmdGui {
    pub page: Page,
    pid_files: FanServices,
    outputs: BTreeMap<String, Vec<Output>>,
    cooling_performance: CoolingPerformance,
    change_fan_settings: ChangeFanSettings,
    outputs_settings: OutputsSettings,
    config: StatefulConfig,
    reload_pid_list_delay: u8,
}

impl epi::App for AmdGui {
    fn update(&mut self, _ctx: &epi::egui::Context, _frame: &Frame) {}

    fn name(&self) -> &str {
        "AMD GUI"
    }
}

impl AmdGui {
    pub fn new_with_config(config: FanConfig) -> Self {
        Self {
            page: Default::default(),
            pid_files: FanServices::from(vec![]),
            outputs: Default::default(),
            cooling_performance: CoolingPerformance::new(100, config.clone()),
            change_fan_settings: ChangeFanSettings::new(config.clone()),
            outputs_settings: OutputsSettings::default(),
            config: StatefulConfig::new(config),
            reload_pid_list_delay: RELOAD_PID_LIST_DELAY,
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        self.config.load_textures(ui);

        match self.page {
            Page::Config => {
                self.change_fan_settings
                    .draw(ui, &mut self.pid_files, &mut self.config);
            }
            Page::Monitoring => {
                self.cooling_performance.draw(ui, &self.pid_files);
            }
            Page::Settings => {}
            Page::Outputs => {
                self.outputs_settings
                    .draw(ui, &mut self.config, &self.outputs);
            }
        }
    }

    pub fn tick(&mut self) {
        self.cooling_performance.tick();
        if self.pid_files.0.is_empty() || self.reload_pid_list_delay.checked_sub(1).is_none() {
            self.reload_pid_list_delay = RELOAD_PID_LIST_DELAY;

            {
                use amdgpu::pidfile::helper_cmd::{send_command, Command, Response};

                match send_command(Command::FanServices) {
                    Ok(Response::Services(services)) if self.pid_files.list_changed(&services) => {
                        self.pid_files = FanServices::from(services);
                    }
                    Ok(Response::Services(_services)) => {
                        // SKIP
                    }
                    Ok(res) => {
                        log::warn!("Unexpected response {:?} while loading fan services", res);
                    }
                    Err(e) => {
                        log::warn!("Failed to load amd fan services pid list. {:?}", e);
                    }
                }
            }

            {
                use amdgpu::pidfile::ports::{send_command, Command, Response};

                match send_command(Command::Ports) {
                    Ok(Response::NoOp) => {}
                    Ok(Response::Ports(outputs)) => {
                        let mut names = outputs.iter().fold(
                            Vec::with_capacity(outputs.len()),
                            |mut set, output| {
                                set.push(output.card.clone());
                                set
                            },
                        );
                        names.sort();

                        let mut tree = BTreeMap::new();
                        names.into_iter().for_each(|name| {
                            tree.insert(name, Vec::with_capacity(6));
                        });

                        self.outputs = outputs.into_iter().fold(tree, |mut agg, output| {
                            let v = agg
                                .entry(output.card.clone())
                                .or_insert_with(|| Vec::with_capacity(6));
                            v.push(output);
                            v.sort_by(|a, b| {
                                format!(
                                    "{}{}{}",
                                    a.port_type,
                                    a.port_name.as_deref().unwrap_or_default(),
                                    a.port_number,
                                )
                                .cmp(&format!(
                                    "{}{}{}",
                                    b.port_type,
                                    b.port_name.as_deref().unwrap_or_default(),
                                    b.port_number,
                                ))
                            });
                            agg
                        });
                    }
                    Err(e) => {
                        log::warn!("Failed to load amd fan services pid list. {:?}", e);
                    }
                }
            }
        } else {
            self.reload_pid_list_delay -= 1;
        }
    }
}
