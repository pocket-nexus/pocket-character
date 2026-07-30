//! Native Persona renderer built from the Pocket character substrate.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use glam::{Mat4, Vec2, Vec3};
use pocket_vrm::{SpringSolver, VrmDoc};
use pocket_widget::{WidgetGame, WindowCommand};
use pocket3d::anim::{Clip, NodeTrs};
use pocket3d::camera::Camera;
use pocket3d::gpu::Gpu;
use pocket3d::hud::Hud;
use pocket3d::input::Input;
use pocket3d::model::{ModelAsset, ModelInstance, ModelLoadOptions};
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use winit::event::MouseButton;

use crate::bridge::{Bridge, BridgeCommand, VoiceState};
use crate::catalog::{ActionRole, Catalog};
use crate::guest::{GuestCommand, GuestEvent, GuestState, PersonaGuest};
use crate::sim::{FaceSim, Pcg32};

const VOICE_IDLE_DELAY: f32 = 0.65;
const ORBIT_RADIANS_PER_PIXEL: f32 = 0.006;
const PAN_UNITS_PER_PIXEL: f32 = 0.0015;

pub struct PersonaConfig {
    pub catalog: Arc<Catalog>,
    pub bundle_path: PathBuf,
    pub bridge_port: Option<u16>,
    pub size: (u32, u32),
    pub max_texture_dim: u32,
}

struct Action {
    name: String,
    role: ActionRole,
    clips: Vec<Clip>,
}

#[derive(Clone, Copy)]
struct Playback {
    action: usize,
    clip: Option<usize>,
    time: f32,
    one_shot: bool,
}

struct RenderRate {
    frames: u32,
    window_start: Instant,
    last_frame: Option<Instant>,
    intervals_ms: Vec<f32>,
    fps: f32,
    p95_ms: f32,
    p99_ms: f32,
    max_ms: f32,
}

impl RenderRate {
    fn new() -> Self {
        Self {
            frames: 0,
            window_start: Instant::now(),
            last_frame: None,
            intervals_ms: Vec::with_capacity(256),
            fps: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
        }
    }

    fn rendered(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.last_frame {
            self.intervals_ms
                .push((now - previous).as_secs_f32() * 1000.0);
        }
        self.last_frame = Some(now);
        self.frames += 1;
        let elapsed = self.window_start.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.frames as f32 / elapsed;
            self.intervals_ms.sort_by(f32::total_cmp);
            self.p95_ms = percentile(&self.intervals_ms, 0.95);
            self.p99_ms = percentile(&self.intervals_ms, 0.99);
            self.max_ms = self.intervals_ms.last().copied().unwrap_or(0.0);
            self.frames = 0;
            self.intervals_ms.clear();
            self.window_start = now;
        }
    }
}

pub struct PersonaWidget {
    config: PersonaConfig,
    bridge: Option<Bridge>,
    guest: Option<PersonaGuest>,
    model: Option<Arc<ModelAsset>>,
    vrm: Option<VrmDoc>,
    actions: Vec<Action>,
    last_clip: Vec<Option<usize>>,
    playback: Option<Playback>,
    fade_from: Option<Vec<NodeTrs>>,
    fade_elapsed: f32,
    fade_duration: f32,
    rng: Pcg32,
    face: FaceSim,
    springs: Option<SpringSolver>,
    locals: Vec<NodeTrs>,
    sampled_locals: Vec<NodeTrs>,
    globals: Vec<Mat4>,
    scene: Scene,
    camera: Camera,
    hud: Hud,
    camera_target: Vec3,
    camera_distance: f32,
    camera_yaw: f32,
    camera_pitch: f32,
    last_cursor: Option<Vec2>,
    last_window_size: (u32, u32),
    voice: VoiceState,
    audio_level: f32,
    voice_idle_delay: Option<f32>,
    pending_events: Vec<(String, String)>,
    window_command: Option<WindowCommand>,
    dirty: bool,
    exit: bool,
    tick_count: u64,
    render_rate: RenderRate,
}

impl PersonaWidget {
    pub fn new(config: PersonaConfig) -> Self {
        let last_window_size = config.size;
        Self {
            config,
            bridge: None,
            guest: None,
            model: None,
            vrm: None,
            actions: Vec::new(),
            last_clip: Vec::new(),
            playback: None,
            fade_from: None,
            fade_elapsed: 0.0,
            fade_duration: 0.0,
            rng: Pcg32::new(0x0070_6572_736f_6e61),
            face: FaceSim::new(0xface_cafe),
            springs: None,
            locals: Vec::new(),
            sampled_locals: Vec::new(),
            globals: Vec::new(),
            scene: Scene::default(),
            camera: Camera::default(),
            hud: Hud::default(),
            camera_target: Vec3::ZERO,
            camera_distance: 1.0,
            camera_yaw: 0.0,
            camera_pitch: 0.0,
            last_cursor: None,
            last_window_size,
            voice: VoiceState::default(),
            audio_level: 0.0,
            voice_idle_delay: None,
            pending_events: Vec::new(),
            window_command: None,
            dirty: true,
            exit: false,
            tick_count: 0,
            render_rate: RenderRate::new(),
        }
    }

    fn active_action_name(&self) -> &str {
        self.playback
            .and_then(|playback| self.actions.get(playback.action))
            .map(|action| action.name.as_str())
            .unwrap_or("idle")
    }

    fn action_index_for_role(&self, role: ActionRole) -> Option<usize> {
        self.actions.iter().position(|action| action.role == role)
    }

    fn action_index(&self, name: &str) -> Option<usize> {
        self.actions.iter().position(|action| action.name == name)
    }

    fn voice_action_index(&self) -> Option<usize> {
        self.action_index_for_role(if self.voice.speaking() {
            ActionRole::Speaking
        } else {
            ActionRole::Idle
        })
    }

    fn choose_clip(&mut self, action_index: usize) -> Option<usize> {
        let count = self.actions.get(action_index)?.clips.len();
        if count == 0 {
            return None;
        }
        let previous = self.last_clip[action_index];
        let mut index = self.rng.index(count);
        if count > 1 && Some(index) == previous {
            index = (index + 1 + self.rng.index(count - 1)) % count;
        }
        self.last_clip[action_index] = Some(index);
        Some(index)
    }

    fn transition_duration(&self, next: usize) -> f32 {
        let Some(previous) = self.playback else {
            return 0.0;
        };
        let previous_role = self.actions[previous.action].role;
        let next_role = self.actions[next].role;
        if previous_role == ActionRole::Speaking && next_role == ActionRole::Idle {
            1.15
        } else if next_role == ActionRole::Speaking {
            0.85
        } else {
            0.7
        }
    }

    fn start_action_index(&mut self, action_index: usize, one_shot: bool) {
        if action_index >= self.actions.len() {
            return;
        }
        let old_name = self.active_action_name().to_string();
        let duration = self.transition_duration(action_index);
        let clip = self.choose_clip(action_index);
        self.fade_from = (duration > 0.0 && !self.locals.is_empty()).then(|| self.locals.clone());
        self.fade_elapsed = 0.0;
        self.fade_duration = duration;
        self.playback = Some(Playback {
            action: action_index,
            clip,
            time: 0.0,
            one_shot,
        });
        let new_name = self.actions[action_index].name.clone();
        if old_name != new_name {
            self.pending_events
                .push(("animationChanged".into(), new_name.clone()));
        }
        if let Some(bridge) = &self.bridge {
            bridge.update_status(|status| status.active_animation = new_name);
        }
        self.dirty = true;
    }

    fn start_action(&mut self, name: &str, one_shot: bool) -> bool {
        let Some(index) = self.action_index(name) else {
            return false;
        };
        if one_shot && self.actions[index].clips.is_empty() {
            return false;
        }
        self.start_action_index(index, one_shot);
        true
    }

    fn resume_voice_action(&mut self) {
        if let Some(index) = self.voice_action_index() {
            self.start_action_index(index, false);
        }
    }

    fn apply_bridge_commands(&mut self) {
        let commands: Vec<_> = self
            .bridge
            .as_ref()
            .map(|bridge| bridge.drain().collect())
            .unwrap_or_default();
        for command in commands {
            match command {
                BridgeCommand::Voice(voice) => {
                    let old_activity = self.voice.activity.clone();
                    self.voice = voice;
                    if old_activity != self.voice.activity {
                        self.pending_events
                            .push(("voiceChanged".into(), self.voice.activity.clone()));
                    }
                    if self.voice.speaking() {
                        self.voice_idle_delay = None;
                        if !self.playback.is_some_and(|playback| playback.one_shot)
                            && let Some(index) = self.action_index_for_role(ActionRole::Speaking)
                            && self
                                .playback
                                .is_none_or(|playback| playback.action != index)
                        {
                            self.start_action_index(index, false);
                        }
                    } else if self.voice.phase == "active" && self.voice.activity == "listening" {
                        self.voice_idle_delay = Some(VOICE_IDLE_DELAY);
                    } else {
                        self.voice_idle_delay = None;
                        if !self.playback.is_some_and(|playback| playback.one_shot) {
                            self.resume_voice_action();
                        }
                    }
                }
                BridgeCommand::AudioLevel(level) => self.audio_level = level,
                BridgeCommand::PlayAnimation(name) => {
                    if !self.start_action(&name, true) {
                        log::warn!("Pocket Persona action is not playable: {name}");
                    }
                }
                BridgeCommand::Window { visible } => {
                    self.window_command = Some(if visible {
                        WindowCommand::Show
                    } else {
                        WindowCommand::Hide
                    });
                }
            }
        }
    }

    fn apply_guest_commands(&mut self, commands: Vec<GuestCommand>) {
        for command in commands {
            match command {
                GuestCommand::PlayAnimation(name) => {
                    if !self.start_action(&name, true) {
                        log::warn!("persona.playAnimation: unknown or empty action '{name}'");
                    }
                }
                GuestCommand::SetExpression(name, weight) => {
                    self.apply_expression(&name, weight.clamp(0.0, 1.0));
                    self.dirty = true;
                }
                GuestCommand::Quit => self.exit = true,
            }
        }
    }

    fn update_voice_delay(&mut self, dt: f32) {
        let Some(delay) = self.voice_idle_delay.as_mut() else {
            return;
        };
        *delay -= dt;
        if *delay <= 0.0 {
            self.voice_idle_delay = None;
            if !self.playback.is_some_and(|playback| playback.one_shot)
                && let Some(index) = self.action_index_for_role(ActionRole::Idle)
                && self
                    .playback
                    .is_none_or(|playback| playback.action != index)
            {
                self.start_action_index(index, false);
            }
        }
    }

    /// Advance body animation and report whether this tick produced a pose
    /// that must be presented. Remember the pre-step fade state so the final
    /// crossfade sample is not lost when `fade_from` is cleared.
    fn sample_animation(&mut self, dt: f32, model: &ModelAsset) -> bool {
        let Some(mut playback) = self.playback else {
            model
                .skeleton
                .sample_locals(None, 0.0, false, &mut self.locals);
            return false;
        };
        playback.time += dt;
        let action = &self.actions[playback.action];
        let clip = playback.clip.and_then(|index| action.clips.get(index));
        let clip_advanced = clip.is_some();
        let fade_advanced = self.fade_from.is_some();
        model.skeleton.sample_locals(
            clip,
            playback.time,
            !playback.one_shot,
            &mut self.sampled_locals,
        );

        if let Some(from) = &self.fade_from {
            self.fade_elapsed += dt;
            let amount = if self.fade_duration <= 0.0 {
                1.0
            } else {
                smoothstep01(self.fade_elapsed / self.fade_duration)
            };
            self.locals.clear();
            self.locals.extend(
                from.iter()
                    .zip(&self.sampled_locals)
                    .map(|(from, to)| blend_trs(*from, *to, amount)),
            );
            if amount >= 1.0 {
                self.fade_from = None;
            }
        } else {
            std::mem::swap(&mut self.locals, &mut self.sampled_locals);
        }
        self.playback = Some(playback);

        if playback.one_shot && clip.is_none_or(|clip| playback.time >= clip.duration) {
            self.playback = None;
            self.resume_voice_action();
        }
        clip_advanced || fade_advanced
    }

    fn apply_expression(&mut self, name: &str, weight: f32) -> bool {
        let (Some(vrm), Some(model), Some(instance)) = (
            self.vrm.as_ref(),
            self.model.as_ref(),
            self.scene.models.first_mut(),
        ) else {
            return false;
        };
        let Some(morph) = instance.morph.as_mut() else {
            return false;
        };
        let Some(expression) = vrm
            .expressions
            .iter()
            .find(|expression| expression.name == name)
        else {
            return false;
        };
        for binding in &expression.binds {
            if let Some(slot) = model.morph_mesh_slot(binding.mesh) {
                morph.set_weight(slot, binding.target, weight * binding.weight);
            }
        }
        true
    }

    fn apply_face(&mut self, dt: f32) -> f32 {
        let outputs = self.face.tick(dt, self.audio_level, self.voice.speaking());
        if outputs.blink_changed {
            self.apply_expression("blink", outputs.blink);
            self.dirty = true;
        }
        if outputs.visemes_changed {
            const VISEMES: [[&str; 2]; 5] = [
                ["aa", "a"],
                ["ee", "e"],
                ["ih", "i"],
                ["oh", "o"],
                ["ou", "u"],
            ];
            for (candidates, weight) in VISEMES.iter().zip(outputs.visemes) {
                if !self.apply_expression(candidates[0], weight) {
                    self.apply_expression(candidates[1], weight);
                }
            }
            self.dirty = true;
        }
        outputs.blink
    }

    fn update_camera(&mut self, input: &Input, window_size: (u32, u32)) {
        let cursor = input.cursor();
        let previous = self.last_cursor;
        self.last_cursor = cursor;
        let mut changed = false;
        if let (Some(cursor), Some(previous)) = (cursor, previous) {
            let delta = cursor - previous;
            if input.mouse_button_down(MouseButton::Left) {
                self.camera_yaw = (self.camera_yaw - delta.x * ORBIT_RADIANS_PER_PIXEL)
                    .rem_euclid(core::f32::consts::TAU);
                self.camera_pitch =
                    (self.camera_pitch - delta.y * ORBIT_RADIANS_PER_PIXEL).clamp(-1.2, 1.2);
                changed = delta != Vec2::ZERO;
            } else if input.mouse_button_down(MouseButton::Right) {
                let scale = self.camera_distance * PAN_UNITS_PER_PIXEL;
                self.camera_target += Vec3::new(-delta.x * scale, delta.y * scale, 0.0);
                changed = delta != Vec2::ZERO;
            }
        }
        let scroll = input.scroll().y;
        if scroll != 0.0 {
            self.camera_distance =
                (self.camera_distance * (-scroll * 0.0015).exp()).clamp(0.4, 20.0);
            changed = true;
        }
        if window_size != self.last_window_size {
            self.last_window_size = window_size;
            changed = true;
        }
        if changed {
            self.rebuild_camera(window_size);
            self.dirty = true;
        }
    }

    fn rebuild_camera(&mut self, window_size: (u32, u32)) {
        let _ = window_size;
        let horizontal = Vec3::new(self.camera_yaw.sin(), 0.0, -self.camera_yaw.cos());
        let direction = Vec3::new(
            horizontal.x * self.camera_pitch.cos(),
            self.camera_pitch.sin(),
            horizontal.z * self.camera_pitch.cos(),
        );
        self.camera.pos = self.camera_target + direction * self.camera_distance;
        self.camera.look_at(self.camera_target);
    }
}

impl WidgetGame for PersonaWidget {
    fn init(&mut self, gpu: &Gpu, renderer: &mut Renderer) -> Result<()> {
        let started = Instant::now();
        let model = ModelAsset::load_glb_opts(
            gpu,
            &renderer.model_material_layout,
            &renderer.samplers,
            &self.config.catalog.model_path,
            &ModelLoadOptions {
                max_texture_dim: Some(self.config.max_texture_dim),
            },
        )
        .context("loading Persona VRM model")?;
        let vrm = VrmDoc::from_path(&self.config.catalog.model_path)
            .context("parsing Persona VRM extension")?;

        self.actions.clear();
        for action in &self.config.catalog.actions {
            let mut clips = Vec::with_capacity(action.clips.len());
            for path in &action.clips {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("reading Persona action {}", path.display()))?;
                let animation = pocket_vrm::load_vrma_bytes(&bytes)
                    .with_context(|| format!("parsing Persona action {}", path.display()))?;
                clips.push(
                    pocket_vrm::retarget(&animation, &vrm.humanoid, &model.skeleton).with_context(
                        || format!("retargeting Persona action {}", path.display()),
                    )?,
                );
            }
            self.actions.push(Action {
                name: action.name.clone(),
                role: action.role,
                clips,
            });
        }
        self.last_clip = vec![None; self.actions.len()];

        model
            .skeleton
            .sample_locals(None, 0.0, false, &mut self.locals);
        self.springs = Some(SpringSolver::new(
            &vrm.springs,
            &model.skeleton,
            &self.locals,
        ));

        let mut instance = ModelInstance::new(model.clone());
        instance.morph = model.create_morph_state(gpu);
        instance.cutout = 0.5;
        instance.lit = 0.25;
        self.scene.transparent_clear = true;
        self.scene.models.push(instance);

        let aabb = model.aabb;
        let center = (aabb.0 + aabb.1) * 0.5;
        let size = aabb.1 - aabb.0;
        self.camera.fov_y = 20f32.to_radians();
        self.camera.znear = 0.01;
        self.camera.zfar = 100.0;
        self.camera_target = center;
        let aspect = self.config.size.0 as f32 / self.config.size.1 as f32;
        let horizontal_fov = 2.0 * ((self.camera.fov_y * 0.5).tan() * aspect).atan();
        let vertical_distance = size.y * 0.5 / (self.camera.fov_y * 0.5).tan();
        let horizontal_distance = size.x * 0.5 / (horizontal_fov * 0.5).tan();
        // Persona's default character_size=1 is passed to its framing helper
        // as a 1.5x zoom, with half the model depth retained as padding.
        self.camera_distance =
            vertical_distance.max(horizontal_distance).max(0.5) * 1.12 / 1.5 + size.z * 0.5;
        self.rebuild_camera(self.config.size);

        let bundle = std::fs::read_to_string(&self.config.bundle_path).with_context(|| {
            format!("reading guest bundle {}", self.config.bundle_path.display())
        })?;
        let action_names: Vec<String> = self
            .actions
            .iter()
            .map(|action| action.name.clone())
            .collect();
        self.guest = Some(PersonaGuest::boot(
            &bundle,
            &self.config.catalog.model_name,
            &action_names,
        )?);
        self.model = Some(model);
        self.vrm = Some(vrm);

        if let Some(index) = self.action_index_for_role(ActionRole::Idle) {
            self.start_action_index(index, false);
        }
        if let Some(port) = self.config.bridge_port {
            self.bridge = Some(Bridge::start(port, self.config.catalog.clone())?);
        }
        log::info!(
            "Pocket Persona initialized in {:.0} ms",
            started.elapsed().as_secs_f32() * 1000.0
        );
        Ok(())
    }

    fn tick(&mut self, dt: f32, input: &Input, window_px: (u32, u32)) -> Result<()> {
        self.tick_count += 1;
        self.apply_bridge_commands();
        self.update_voice_delay(dt);
        self.update_camera(input, window_px);
        let Some(model) = self.model.clone() else {
            return Ok(());
        };
        let pose_advanced = self.sample_animation(dt, &model);
        let spring_advanced = if let Some(springs) = self.springs.as_mut() {
            let advanced = springs.joint_count() > 0;
            springs.step(dt, &model.skeleton, &mut self.locals, Mat4::IDENTITY);
            advanced
        } else {
            false
        };
        self.globals = self.scene.models[0].pose.take().unwrap_or_default();
        model
            .skeleton
            .globals_from_locals(&self.locals, &mut self.globals);
        self.scene.models[0].pose = Some(std::mem::take(&mut self.globals));
        let blink = self.apply_face(dt);

        let event_storage = std::mem::take(&mut self.pending_events);
        let events: Vec<GuestEvent<'_>> = event_storage
            .iter()
            .map(|(kind, value)| GuestEvent { kind, value })
            .collect();
        let guest_state = GuestState {
            t: self.tick_count as f64 * dt as f64,
            activity: &self.voice.activity,
            audio_level: self.audio_level,
            animation: self.active_action_name(),
            blink,
            render_fps: self.render_rate.fps,
        };
        let commands = if let Some(guest) = &self.guest {
            guest.turn(&guest_state, &events)?
        } else {
            Vec::new()
        };
        drop(events);
        drop(event_storage);
        self.apply_guest_commands(commands);

        if pose_advanced || spring_advanced {
            self.dirty = true;
        }
        if let Some(bridge) = &self.bridge {
            let active_animation = self.active_action_name().to_string();
            bridge.update_status(|status| {
                status.voice_state = self.voice.clone();
                status.audio_level = self.audio_level;
                status.active_animation = active_animation;
                status.render_fps = self.render_rate.fps;
                status.frame_time_p95_ms = self.render_rate.p95_ms;
                status.frame_time_p99_ms = self.render_rate.p99_ms;
                status.frame_time_max_ms = self.render_rate.max_ms;
            });
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn prepare(&mut self, _gpu: &Gpu) -> Result<()> {
        Ok(())
    }

    fn compose(&mut self, time: f32, _size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.scene.time = time;
        self.render_rate.rendered();
        (&self.scene, &self.camera, &self.hud)
    }

    fn drag_at(&mut self, _cursor: Vec2) -> bool {
        false
    }

    fn take_window_command(&mut self) -> Option<WindowCommand> {
        self.window_command.take()
    }

    fn wants_exit(&self) -> bool {
        self.exit
    }
}

fn blend_trs(from: NodeTrs, to: NodeTrs, amount: f32) -> NodeTrs {
    NodeTrs {
        translation: from.translation.lerp(to.translation, amount),
        rotation: from.rotation.slerp(to.rotation, amount),
        scale: from.scale.lerp(to.scale, amount),
    }
}

fn smoothstep01(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

fn percentile(sorted: &[f32], quantile: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f32 * quantile)
        .round()
        .clamp(0.0, (sorted.len() - 1) as f32) as usize;
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_blend_preserves_endpoints() {
        let from = NodeTrs {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: glam::Quat::IDENTITY,
            scale: Vec3::ONE,
        };
        let to = NodeTrs {
            translation: Vec3::new(4.0, 5.0, 6.0),
            rotation: glam::Quat::from_rotation_y(1.0),
            scale: Vec3::splat(2.0),
        };
        assert_eq!(blend_trs(from, to, 0.0).translation, from.translation);
        assert_eq!(blend_trs(from, to, 1.0).translation, to.translation);
        assert_eq!(blend_trs(from, to, 0.5).scale, Vec3::splat(1.5));
    }

    #[test]
    fn transition_curve_is_bounded() {
        assert_eq!(smoothstep01(-1.0), 0.0);
        assert_eq!(smoothstep01(0.0), 0.0);
        assert_eq!(smoothstep01(1.0), 1.0);
        assert_eq!(smoothstep01(2.0), 1.0);
        assert!((smoothstep01(0.5) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn frame_percentiles_are_selected_from_sorted_intervals() {
        assert_eq!(percentile(&[], 0.95), 0.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 3.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.95), 4.0);
    }
}
