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

const VOICE_IDLE_DELAY: f32 = 0.9;
const BODY_TRANSITION_SECONDS: f32 = 0.35;
const SPEAKING_CHUNK_HALF_BASE_SECONDS: f32 = 0.45;
const SPEAKING_CHUNK_FACTOR_MIN: f32 = 1.5;
const SPEAKING_CHUNK_FACTOR_MAX: f32 = 1.8;
const SPEAKING_RESUME_HOLD_SECONDS: f32 = 0.7;
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
    next_chunk_at: Option<f32>,
}

struct RenderRate {
    frames: u32,
    total_frames: u64,
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
            total_frames: 0,
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
        self.total_frames += 1;
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
    automatic_role: ActionRole,
    audio_level: f32,
    has_observed_audio_level: bool,
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
            automatic_role: ActionRole::Idle,
            audio_level: 0.0,
            has_observed_audio_level: false,
            voice_idle_delay: None,
            pending_events: Vec::new(),
            window_command: None,
            dirty: true,
            exit: false,
            tick_count: 0,
            render_rate: RenderRate::new(),
        }
    }

    /// Deterministic offscreen receipt hook. Windowed/benchmark runs still
    /// enter this state through the same Persona-compatible event bridge.
    pub(crate) fn set_headless_speaking(&mut self) {
        self.voice = VoiceState {
            phase: "active".into(),
            activity: "speaking".into(),
            microphone_muted: false,
            output_muted: false,
        };
        self.automatic_role = ActionRole::Speaking;
        self.audio_level = 0.35;
        self.has_observed_audio_level = true;
        self.voice_idle_delay = None;
        if let Some(index) = self.automatic_action_index() {
            self.start_action_index(index, false);
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

    fn automatic_action_index(&self) -> Option<usize> {
        self.action_index_for_role(self.automatic_role)
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

    fn transition_duration(&self, _next: usize) -> f32 {
        if self.playback.is_some() {
            BODY_TRANSITION_SECONDS
        } else {
            0.0
        }
    }

    fn speaking_transition_duration(&mut self) -> f32 {
        SPEAKING_CHUNK_HALF_BASE_SECONDS
            * (self
                .rng
                .range(SPEAKING_CHUNK_FACTOR_MIN, SPEAKING_CHUNK_FACTOR_MAX)
                + self
                    .rng
                    .range(SPEAKING_CHUNK_FACTOR_MIN, SPEAKING_CHUNK_FACTOR_MAX))
    }

    fn speaking_chunk_schedule(
        &mut self,
        action_index: usize,
        clip_index: Option<usize>,
        one_shot: bool,
    ) -> (Option<f32>, f32) {
        let action = &self.actions[action_index];
        if one_shot || action.role != ActionRole::Speaking || action.clips.len() <= 1 {
            return (None, 0.0);
        }
        let clip_duration = clip_index
            .and_then(|index| action.clips.get(index))
            .map_or(0.0, |clip| clip.duration);
        let fade = self.speaking_transition_duration();
        (Some(speaking_chunk_dwell(clip_duration, fade)), fade)
    }

    fn start_action_index(&mut self, action_index: usize, one_shot: bool) {
        if action_index >= self.actions.len() {
            return;
        }
        let old_name = self.active_action_name().to_string();
        let duration = self.transition_duration(action_index);
        let clip = self.choose_clip(action_index);
        let (next_chunk_at, _) = self.speaking_chunk_schedule(action_index, clip, one_shot);
        self.fade_from = (duration > 0.0 && !self.locals.is_empty()).then(|| self.locals.clone());
        self.fade_elapsed = 0.0;
        self.fade_duration = duration;
        self.playback = Some(Playback {
            action: action_index,
            clip,
            time: 0.0,
            one_shot,
            next_chunk_at,
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

    fn resume_automatic_action(&mut self) {
        if let Some(index) = self.automatic_action_index() {
            self.start_action_index(index, false);
        }
    }

    fn advance_speaking_chunk(&mut self, action_index: usize) {
        let Some(clip) = self.choose_clip(action_index) else {
            return;
        };
        let (next_chunk_at, fade_duration) =
            self.speaking_chunk_schedule(action_index, Some(clip), false);
        self.fade_from = (!self.locals.is_empty()).then(|| self.locals.clone());
        self.fade_elapsed = 0.0;
        self.fade_duration = fade_duration;
        self.playback = Some(Playback {
            action: action_index,
            clip: Some(clip),
            time: 0.0,
            one_shot: false,
            next_chunk_at,
        });
        self.dirty = true;
    }

    fn hold_speaking_chunk_after_resume(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let modular_speaking = !playback.one_shot
            && self.actions.get(playback.action).is_some_and(|action| {
                action.role == ActionRole::Speaking && action.clips.len() > 1
            });
        if modular_speaking && let Some(deadline) = playback.next_chunk_at.as_mut() {
            *deadline = speaking_resume_deadline(*deadline, playback.time);
        }
    }

    fn speaking_motion_active(&self) -> bool {
        speaking_motion_active(&self.voice, self.has_observed_audio_level, self.audio_level)
    }

    fn hold_if_speaking_motion_resumed(&mut self, was_active: bool) {
        if !was_active && self.speaking_motion_active() {
            self.hold_speaking_chunk_after_resume();
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
                    let motion_was_active = self.speaking_motion_active();
                    self.voice = voice;
                    if self.voice.phase == "inactive" {
                        self.has_observed_audio_level = false;
                    }
                    if old_activity != self.voice.activity {
                        self.pending_events
                            .push(("voiceChanged".into(), self.voice.activity.clone()));
                    }
                    match immediate_automatic_role(&self.voice) {
                        Some(role) => {
                            self.automatic_role = role;
                            self.voice_idle_delay = None;
                            if role == ActionRole::Idle {
                                self.audio_level = 0.0;
                            }
                            if !self.playback.is_some_and(|playback| playback.one_shot)
                                && let Some(index) = self.automatic_action_index()
                                && self
                                    .playback
                                    .is_none_or(|playback| playback.action != index)
                            {
                                self.start_action_index(index, false);
                            }
                        }
                        None => {
                            // Active idle/listening keeps the current automatic
                            // body role until the sentence-gap timer expires.
                            self.voice_idle_delay = Some(VOICE_IDLE_DELAY);
                        }
                    }
                    self.hold_if_speaking_motion_resumed(motion_was_active);
                }
                BridgeCommand::AudioLevel(level) => {
                    let motion_was_active = self.speaking_motion_active();
                    self.audio_level = level;
                    self.has_observed_audio_level = true;
                    self.hold_if_speaking_motion_resumed(motion_was_active);
                }
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
            self.automatic_role = ActionRole::Idle;
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
        let modular_speaking =
            !playback.one_shot && action.role == ActionRole::Speaking && action.clips.len() > 1;
        let sample_time = if modular_speaking {
            clip.map_or(playback.time, |clip| {
                ping_pong_sample_time(playback.time, clip.duration)
            })
        } else {
            playback.time
        };
        let clip_advanced = clip.is_some();
        let fade_advanced = self.fade_from.is_some();
        model.skeleton.sample_locals(
            clip,
            sample_time,
            !playback.one_shot && !modular_speaking,
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
            self.resume_automatic_action();
        } else if self.speaking_motion_active()
            && playback
                .next_chunk_at
                .is_some_and(|deadline| playback.time >= deadline)
        {
            self.advance_speaking_chunk(playback.action);
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
                status.render_frame_count = self.render_rate.total_frames;
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

fn speaking_chunk_dwell(clip_duration: f32, transition_duration: f32) -> f32 {
    clip_duration.max(transition_duration + 0.5)
}

fn ping_pong_sample_time(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        return 0.0;
    }
    let period = duration * 2.0;
    let phase = elapsed.rem_euclid(period);
    if phase <= duration {
        phase
    } else {
        period - phase
    }
}

fn speaking_resume_deadline(next_transition_at: f32, playback_time: f32) -> f32 {
    next_transition_at.max(playback_time + SPEAKING_RESUME_HOLD_SECONDS)
}

fn speaking_motion_active(
    voice: &VoiceState,
    has_observed_audio_level: bool,
    audio_level: f32,
) -> bool {
    voice.speaking() && (!has_observed_audio_level || audio_level > 0.0)
}

fn immediate_automatic_role(voice: &VoiceState) -> Option<ActionRole> {
    if voice.phase != "active" || voice.output_muted {
        Some(ActionRole::Idle)
    } else if voice.activity == "speaking" {
        Some(ActionRole::Speaking)
    } else {
        None
    }
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

    #[test]
    fn speaking_chunk_dwell_preserves_gesture_and_blend() {
        assert_eq!(speaking_chunk_dwell(3.0, 1.5), 3.0);
        assert_eq!(speaking_chunk_dwell(1.0, 1.5), 2.0);
    }

    #[test]
    fn speaking_chunks_ping_pong_instead_of_wrapping() {
        assert_eq!(ping_pong_sample_time(0.75, 1.0), 0.75);
        assert_eq!(ping_pong_sample_time(1.0, 1.0), 1.0);
        assert_eq!(ping_pong_sample_time(1.25, 1.0), 0.75);
        assert_eq!(ping_pong_sample_time(1.5, 1.0), 0.5);
        assert_eq!(ping_pong_sample_time(2.25, 1.0), 0.25);
        assert_eq!(ping_pong_sample_time(1.0, 0.0), 0.0);
    }

    #[test]
    fn speaking_resume_holds_the_current_chunk() {
        assert_eq!(speaking_resume_deadline(4.0, 2.0), 4.0);
        assert_eq!(speaking_resume_deadline(2.1, 2.0), 2.7);
    }

    #[test]
    fn body_transitions_match_latest_persona_default() {
        assert_eq!(BODY_TRANSITION_SECONDS, 0.35);
    }

    #[test]
    fn audio_observation_pauses_and_resumes_speaking_motion() {
        let voice = VoiceState {
            phase: "active".into(),
            activity: "speaking".into(),
            microphone_muted: false,
            output_muted: false,
        };
        assert!(speaking_motion_active(&voice, false, 0.0));
        assert!(!speaking_motion_active(&voice, true, 0.0));
        assert!(speaking_motion_active(&voice, true, 0.1));

        let mut muted = voice.clone();
        muted.output_muted = true;
        assert!(!speaking_motion_active(&muted, true, 0.1));
    }

    #[test]
    fn automatic_role_holds_during_active_idle_and_listening() {
        for activity in ["idle", "listening"] {
            let voice = VoiceState {
                phase: "active".into(),
                activity: activity.into(),
                microphone_muted: false,
                output_muted: false,
            };
            assert_eq!(immediate_automatic_role(&voice), None);
        }
        assert_eq!(VOICE_IDLE_DELAY, 0.9);

        let mut inactive = VoiceState::default();
        assert_eq!(immediate_automatic_role(&inactive), Some(ActionRole::Idle));
        inactive.phase = "active".into();
        inactive.output_muted = true;
        assert_eq!(immediate_automatic_role(&inactive), Some(ActionRole::Idle));

        inactive.output_muted = false;
        inactive.activity = "speaking".into();
        assert_eq!(
            immediate_automatic_role(&inactive),
            Some(ActionRole::Speaking)
        );
    }
}
