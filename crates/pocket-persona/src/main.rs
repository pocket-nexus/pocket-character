//! Pocket Persona: a native renderer vertical slice compatible with Persona's
//! asset catalog, local event bridge, and MCP tools.

mod bridge;
mod catalog;
mod guest;
mod sim;
mod widget;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use pocket_widget::{WidgetConfig, WidgetGame};
use pocket3d::gpu::{Gpu, OffscreenTarget};
use pocket3d::input::Input;
use pocket3d::renderer::Renderer;

use catalog::Catalog;
use widget::{PersonaConfig, PersonaWidget};

const DEFAULT_SIZE: (u32, u32) = (430, 680);

struct Args {
    library: PathBuf,
    bundle: PathBuf,
    bridge_port: Option<u16>,
    fps: f32,
    size: (u32, u32),
    max_texture_dim: u32,
    headless_shot: Option<PathBuf>,
    ticks: u32,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args(std::env::args().skip(1).collect())?;
    let catalog = Arc::new(Catalog::from_path(&args.library)?);
    let widget = PersonaWidget::new(PersonaConfig {
        catalog,
        bundle_path: args.bundle,
        bridge_port: if args.headless_shot.is_some() {
            None
        } else {
            args.bridge_port
        },
        size: args.size,
        max_texture_dim: args.max_texture_dim,
    });
    if let Some(output) = args.headless_shot {
        return headless_shot(widget, args.size, args.fps, args.ticks, &output);
    }
    pocket_widget::run(
        WidgetConfig {
            title: "Pocket Persona".into(),
            size: args.size,
            tick_hz: args.fps,
            max_fps: args.fps,
            transparent: true,
            decorations: false,
            always_on_top: true,
            resizable: true,
            min_size: (320, 480),
            ime: false,
        },
        widget,
    )
}

fn parse_args(values: Vec<String>) -> Result<Args> {
    let mut library = None;
    let mut bundle = default_repo_root().join("dist/pocket-persona/guest.js");
    let mut bridge_port = Some(47_831);
    let mut fps = 60.0;
    let mut size = DEFAULT_SIZE;
    let mut max_texture_dim = 2048;
    let mut headless_shot = None;
    let mut ticks = 90;
    let mut index = 0;
    while index < values.len() {
        let flag = &values[index];
        let next = |index: &mut usize| -> Result<&str> {
            *index += 1;
            values
                .get(*index)
                .map(String::as_str)
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--library" => library = Some(PathBuf::from(next(&mut index)?)),
            "--bundle" => bundle = PathBuf::from(next(&mut index)?),
            "--bridge-port" => bridge_port = Some(next(&mut index)?.parse()?),
            "--no-bridge" => bridge_port = None,
            "--fps" | "--max-fps" => fps = next(&mut index)?.parse()?,
            "--size" => size = parse_size(next(&mut index)?)?,
            "--max-texture-dim" => max_texture_dim = next(&mut index)?.parse()?,
            "--headless-shot" => headless_shot = Some(PathBuf::from(next(&mut index)?)),
            "--ticks" => ticks = next(&mut index)?.parse()?,
            "--help" | "-h" => {
                println!(
                    "Pocket Persona\n\
                     \n\
                     Usage: pocket-persona --library <library.json> [options]\n\
                     \n\
                     Options:\n\
                     \t--bundle <guest.js>          Pocket policy bundle\n\
                     \t--bridge-port <port>         Persona HTTP/MCP port (default 47831; 0 = any)\n\
                     \t--no-bridge                  Disable HTTP/MCP\n\
                     \t--fps <hz>                   Fixed update/render cap (default 60)\n\
                     \t--size <width>x<height>      Logical window size (default 430x680)\n\
                     \t--max-texture-dim <pixels>   Texture cap (default 2048)\n\
                     \t--headless-shot <png>        Render one offscreen verification frame\n\
                     \t--ticks <count>              Headless fixed steps (default 90)"
                );
                std::process::exit(0);
            }
            _ => bail!("unknown Pocket Persona argument: {flag}"),
        }
        index += 1;
    }
    if !(1.0..=240.0).contains(&fps) {
        bail!("--fps must be between 1 and 240");
    }
    if !(256..=4096).contains(&max_texture_dim) {
        bail!("--max-texture-dim must be between 256 and 4096");
    }
    let library = library.context("--library is required")?;
    Ok(Args {
        library,
        bundle,
        bridge_port,
        fps,
        size,
        max_texture_dim,
        headless_shot,
        ticks,
    })
}

fn parse_size(value: &str) -> Result<(u32, u32)> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .context("--size must look like 430x680")?;
    let size = (width.parse()?, height.parse()?);
    if size.0 < 64 || size.1 < 64 || size.0 > 4096 || size.1 > 4096 {
        bail!("--size dimensions must be between 64 and 4096");
    }
    Ok(size)
}

fn default_repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn headless_shot(
    mut widget: PersonaWidget,
    size: (u32, u32),
    fps: f32,
    ticks: u32,
    output: &Path,
) -> Result<()> {
    let gpu = Gpu::new_headless()?;
    let mut renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT)?;
    widget.init(&gpu, &mut renderer)?;
    let input = Input::default();
    for _ in 0..ticks {
        widget.tick(1.0 / fps, &input, size)?;
    }
    widget.prepare(&gpu)?;
    let (scene, camera, hud) = widget.compose(ticks as f32 / fps, size);
    let target = OffscreenTarget::new(&gpu, size.0, size.1);
    renderer.render(&gpu, &target.view, size, scene, camera, hud);
    target.save_png(&gpu, output)?;
    println!("wrote {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_window_size() {
        assert_eq!(parse_size("430x680").unwrap(), (430, 680));
        assert!(parse_size("430").is_err());
        assert!(parse_size("20x20").is_err());
    }
}
