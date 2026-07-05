//! Software rendering of the virtual output (for the monitor web UI) and of
//! individual windows (for MCP screenshots), using smithay's pixman renderer.

use std::io::Cursor;
use std::sync::Mutex;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{surface::WaylandSurfaceRenderElement, AsRenderElements},
            pixman::PixmanRenderer,
            utils::draw_render_elements,
            Bind, Color32F, ExportMem, Frame, Offscreen, Renderer,
        },
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Physical, Point, Rectangle, Size, Transform},
};

const CLEAR_COLOR: Color32F = Color32F::new(0.13, 0.14, 0.17, 1.0);
/// All buffers use ARGB8888 (pixman a8r8g8b8): little-endian BGRA bytes.
const FORMAT: Fourcc = Fourcc::Argb8888;

pub struct FrameData {
    pub rgba: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub seq: u64,
}

/// Latest composited frame, shared with the HTTP monitor.
pub struct FrameStore {
    inner: Mutex<FrameData>,
}

impl FrameStore {
    pub fn new() -> Self {
        FrameStore {
            inner: Mutex::new(FrameData {
                rgba: Vec::new(),
                width: 0,
                height: 0,
                seq: 0,
            }),
        }
    }

    pub fn store(&self, rgba: Vec<u8>, width: i32, height: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner.rgba = rgba;
        inner.width = width;
        inner.height = height;
        inner.seq += 1;
    }

    #[allow(dead_code)]
    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    pub fn png(&self) -> Option<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        if inner.rgba.is_empty() {
            return None;
        }
        encode_png(&inner.rgba, inner.width, inner.height)
    }
}

fn bgra_to_rgba(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255; // discard alpha; the desktop is opaque
    }
    out
}

fn encode_png(rgba: &[u8], width: i32, height: i32) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Render the whole desktop into the shared frame store (monitor view).
/// Also draws a small crosshair at the pointer position.
pub fn render_desktop_frame(
    renderer: &mut PixmanRenderer,
    space: &Space<Window>,
    output: &Output,
    damage_tracker: &mut OutputDamageTracker,
    size: Size<i32, smithay::utils::Logical>,
    pointer: Point<f64, smithay::utils::Logical>,
    frames: &FrameStore,
) -> anyhow::Result<()> {
    let buffer_size = Size::<i32, smithay::utils::Buffer>::from((size.w, size.h));
    let mut target: smithay::reexports::pixman::Image<'static, 'static> =
        renderer.create_buffer(FORMAT, buffer_size)?;
    {
        let mut fb = renderer.bind(&mut target)?;
        smithay::desktop::space::render_output::<
            _,
            WaylandSurfaceRenderElement<PixmanRenderer>,
            _,
            _,
        >(
            output,
            renderer,
            &mut fb,
            1.0,
            0,
            [space],
            &[],
            damage_tracker,
            CLEAR_COLOR,
        )?;
        let mapping = renderer.copy_framebuffer(
            &fb,
            Rectangle::from_size(buffer_size),
            FORMAT,
        )?;
        let bytes = renderer.map_texture(&mapping)?;
        let mut rgba = bgra_to_rgba(bytes);
        draw_crosshair(&mut rgba, size.w, size.h, pointer.x as i32, pointer.y as i32);
        frames.store(rgba, size.w, size.h);
    }
    Ok(())
}

fn draw_crosshair(rgba: &mut [u8], w: i32, h: i32, cx: i32, cy: i32) {
    let mut put = |x: i32, y: i32| {
        if x >= 0 && x < w && y >= 0 && y < h {
            let i = ((y * w + x) * 4) as usize;
            rgba[i] = 255;
            rgba[i + 1] = 60;
            rgba[i + 2] = 60;
            rgba[i + 3] = 255;
        }
    };
    for d in -8..=8 {
        put(cx + d, cy);
        put(cx, cy + d);
    }
}

/// Render a single window (with its popups) into a PNG, cropped to its
/// geometry so the image matches the window-relative coordinate system.
pub fn capture_window(
    renderer: &mut PixmanRenderer,
    window: &Window,
    size: Size<i32, smithay::utils::Logical>,
) -> Option<Vec<u8>> {
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    let buffer_size = Size::<i32, smithay::utils::Buffer>::from((size.w, size.h));
    let physical_size = Size::<i32, Physical>::from((size.w, size.h));
    let mut target: smithay::reexports::pixman::Image<'static, 'static> =
        renderer.create_buffer(FORMAT, buffer_size).ok()?;

    // Position the window's geometry origin at (0, 0).
    let geo = window.geometry();
    let location = Point::<i32, Physical>::from((-geo.loc.x, -geo.loc.y));
    let elements: Vec<WaylandSurfaceRenderElement<PixmanRenderer>> =
        window.render_elements(renderer, location, 1.0.into(), 1.0);

    let full = Rectangle::from_size(physical_size);
    let rgba = {
        let mut fb = renderer.bind(&mut target).ok()?;
        {
            let mut frame = renderer
                .render(&mut fb, physical_size, Transform::Normal)
                .ok()?;
            frame.clear(CLEAR_COLOR, &[full]).ok()?;
            draw_render_elements(&mut frame, 1.0, &elements, &[full]).ok()?;
            let _sync = frame.finish().ok()?;
        }
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size(buffer_size), FORMAT)
            .ok()?;
        let bytes = renderer.map_texture(&mapping).ok()?;
        bgra_to_rgba(bytes)
    };
    encode_png(&rgba, size.w, size.h)
}
