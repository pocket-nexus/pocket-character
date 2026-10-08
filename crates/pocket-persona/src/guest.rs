//! Pocket `persona` surface: bounded facts into QuickJS, queued intents out.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Result, anyhow};
use pocket_mod::Guest;
use pocket_mod::qjs::{Array, Function, Object};

#[derive(Clone, Debug)]
pub enum GuestCommand {
    PlayAnimation(String),
    SetExpression(String, f32),
    Quit,
}

pub struct GuestState<'a> {
    pub t: f64,
    pub activity: &'a str,
    pub audio_level: f32,
    pub animation: &'a str,
    pub blink: f32,
    pub render_fps: f32,
}

pub struct GuestEvent<'a> {
    pub kind: &'a str,
    pub value: &'a str,
}

pub struct PersonaGuest {
    guest: Guest,
    commands: Rc<RefCell<Vec<GuestCommand>>>,
}

impl PersonaGuest {
    pub fn boot(bundle: &str, model_name: &str, action_names: &[String]) -> Result<PersonaGuest> {
        let guest = Guest::new()?;
        let commands: Rc<RefCell<Vec<GuestCommand>>> = Rc::default();
        let queued = commands.clone();
        let model_name = model_name.to_string();
        let action_names = action_names.to_vec();
        guest.mount("persona", move |ctx, namespace| {
            let boot = Object::new(ctx.clone())?;
            boot.set("model", model_name.as_str())?;
            boot.set("actions", action_names.clone())?;
            namespace.set("boot", boot)?;

            let queue = queued.clone();
            namespace.set(
                "playAnimation",
                Function::new(ctx.clone(), move |name: String| {
                    queue.borrow_mut().push(GuestCommand::PlayAnimation(name));
                })?,
            )?;
            let queue = queued.clone();
            namespace.set(
                "setExpression",
                Function::new(ctx.clone(), move |name: String, weight: f64| {
                    queue
                        .borrow_mut()
                        .push(GuestCommand::SetExpression(name, weight as f32));
                })?,
            )?;
            let queue = queued.clone();
            namespace.set(
                "quit",
                Function::new(ctx.clone(), move || {
                    queue.borrow_mut().push(GuestCommand::Quit);
                })?,
            )?;
            Ok(())
        })?;
        guest.eval("pocket-persona", bundle)?;
        Ok(Self { guest, commands })
    }

    pub fn turn(
        &self,
        state: &GuestState<'_>,
        events: &[GuestEvent<'_>],
    ) -> Result<Vec<GuestCommand>> {
        self.guest.with(|ctx| -> Result<()> {
            let namespace: Object = ctx.globals().get("persona")?;
            let Ok(dispatch) = namespace.get::<_, Function>("__dispatch") else {
                return Ok(());
            };
            let js_state = Object::new(ctx.clone())?;
            js_state.set("t", state.t)?;
            js_state.set("activity", state.activity)?;
            js_state.set("audioLevel", state.audio_level as f64)?;
            js_state.set("animation", state.animation)?;
            js_state.set("blink", state.blink as f64)?;
            js_state.set("renderFps", state.render_fps as f64)?;
            let js_events = Array::new(ctx.clone())?;
            for (index, event) in events.iter().enumerate() {
                let js_event = Object::new(ctx.clone())?;
                js_event.set("type", event.kind)?;
                js_event.set("value", event.value)?;
                js_events.set(index, js_event)?;
            }
            dispatch
                .call::<_, ()>((js_state, js_events))
                .map_err(|error| anyhow!("persona.__dispatch threw: {error}"))
        })?;
        self.guest.frame(0)?;
        Ok(self.commands.borrow_mut().drain(..).collect())
    }
}
