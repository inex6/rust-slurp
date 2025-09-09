use std::process::exit;
use std::os::unix::io::{AsRawFd, BorrowedFd};

use cairo::{Context, Format, ImageSurface};
use memmap2::MmapMut;

use wayland_client::protocol::{
    wl_compositor,
    wl_keyboard,
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
    wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut state = State {
        running: true,
        exit_code: 0,
        qh: qh.clone(),
        compositor: None,
        shm: None,
        layer_shell: None,
        xdg_output_manager: None,
        seat: None,
        pointer: None,
        keyboard: None,
        outputs: Vec::new(),
        start_pos: None,
        current_pos: (0.0, 0.0),
        current_output: None,
        selections: Vec::new(),
    };

    // First roundtrip to get globals
    event_queue.roundtrip(&mut state).unwrap();

    if state.compositor.is_none() || state.shm.is_none() || state.layer_shell.is_none() || state.seat.is_none() || state.xdg_output_manager.is_none() {
        eprintln!("Error: Your compositor does not support the required Wayland protocols.");
        eprintln!("Missing: {} {} {} {} {}",
            if state.compositor.is_none() { "wl_compositor" } else { "" },
            if state.shm.is_none() { "wl_shm" } else { "" },
            if state.layer_shell.is_none() { "zwlr_layer_shell_v1" } else { "" },
            if state.seat.is_none() { "wl_seat" } else { "" },
            if state.xdg_output_manager.is_none() { "zxdg_output_manager_v1" } else { "" }
        );
        exit(1);
    }

    // Second roundtrip to get output info
    event_queue.roundtrip(&mut state).unwrap();

    while state.running {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }

    exit(state.exit_code);
}

struct State {
    running: bool,
    exit_code: i32,
    qh: QueueHandle<Self>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    outputs: Vec<OutputState>,
    start_pos: Option<(f64, f64)>,
    current_pos: (f64, f64),
    current_output: Option<usize>,
    selections: Vec<(f64, f64, f64, f64)>,
}

struct OutputState {
    output: WlOutput,
    xdg_output: zxdg_output_v1::ZxdgOutputV1,
    logical_pos: (i32, i32),
    size: (u32, u32),
    surface: wl_surface::WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffer: Option<Buffer>,
}

struct Buffer {
    pool: WlShmPool,
    width: i32,
    height: i32,
    _file: std::fs::File,
    mmap: MmapMut,
}

impl State {
    fn draw(&mut self) {
        for i in 0..self.outputs.len() {
            self.draw_on_output(i);
        }
    }

    fn draw_on_output(&mut self, output_index: usize) {
        let selections = self.selections.clone();
        let start_pos = self.start_pos;
        let current_pos = self.current_pos;

        if let Some(output_state) = self.outputs.get_mut(output_index) {
            if let Some(buffer) = output_state.buffer.as_mut() {
                let width = buffer.width;
                let height = buffer.height;
                let stride = cairo::Format::ARgb32.stride_for_width(width as u32).unwrap();
                let output_pos = output_state.logical_pos;

                let wl_surface = &output_state.surface;
                let wl_buffer = buffer.pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &self.qh, ());

                {
                    let mmap = &mut buffer.mmap[..];
                    let surface = unsafe { ImageSurface::create_for_data_unsafe(mmap.as_mut_ptr(), Format::ARgb32, width, height, stride).unwrap() };
                    let ctx = Context::new(&surface).unwrap();

                    // Draw semi-transparent background
                    ctx.set_source_rgba(0.5, 0.5, 0.5, 0.4);
                    ctx.set_operator(cairo::Operator::Source);
                    ctx.paint().unwrap();

                    ctx.set_operator(cairo::Operator::Over);

                    let mut all_selections = selections;
                    if let Some(start) = start_pos {
                        let current_selection = get_selection_box(start, current_pos);
                        all_selections.push(current_selection);
                    }
                    draw_selections(&ctx, &all_selections, output_pos);

                    // Translate global mouse pos to local
                    let local_mouse_x = current_pos.0 - output_pos.0 as f64;
                    let local_mouse_y = current_pos.1 - output_pos.1 as f64;

                    // Draw crosshair at current mouse position
                    let crosshair_size = 10.0;
                    let crosshair_width = 1.0;
                    ctx.set_source_rgb(1.0, 1.0, 1.0);
                    ctx.set_line_width(crosshair_width);
                    ctx.move_to(local_mouse_x - crosshair_size, local_mouse_y);
                    ctx.line_to(local_mouse_x + crosshair_size, local_mouse_y);
                    ctx.stroke().unwrap();
                    ctx.move_to(local_mouse_x, local_mouse_y - crosshair_size);
                    ctx.line_to(local_mouse_x, local_mouse_y + crosshair_size);
                    ctx.stroke().unwrap();

                    surface.flush();
                }

                wl_surface.attach(Some(&wl_buffer), 0, 0);
                wl_surface.damage_buffer(0, 0, width, height);
                wl_surface.commit();
                wl_buffer.destroy();
            }
        }
    }
}

fn draw_selections(ctx: &Context, selections: &[(f64, f64, f64, f64)], output_pos: (i32, i32)) {
    for &(gx, gy, gw, gh) in selections {
        let local_x = gx - output_pos.0 as f64;
        let local_y = gy - output_pos.1 as f64;

        // Clear the selection area
        ctx.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        ctx.set_operator(cairo::Operator::Source);
        ctx.rectangle(local_x, local_y, gw, gh);
        ctx.fill().unwrap();

        // Draw selection border
        ctx.set_operator(cairo::Operator::Over);
        ctx.set_source_rgba(0.2, 0.6, 1.0, 0.8);
        ctx.set_line_width(2.0);
        ctx.rectangle(local_x, local_y, gw, gh);
        ctx.stroke().unwrap();
    }
}

fn get_selection_box(p1: (f64, f64), p2: (f64, f64)) -> (f64, f64, f64, f64) {
    let x = p1.0.min(p2.0);
    let y = p1.1.min(p2.1);
    let w = (p1.0 - p2.0).abs();
    let h = (p1.1 - p2.1).abs();
    (x, y, w, h)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_output_manager = Some(registry.bind(name, version, qh, ()));
                }
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    state.pointer = Some(seat.get_pointer(qh, ()));
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                    state.seat = Some(seat);
                }
                "wl_output" => {
                    let output: WlOutput = registry.bind(name, version, qh, ());
                    let surface = state.compositor.as_ref().unwrap().create_surface(qh, ());
                    let layer_surface = state.layer_shell.as_ref().unwrap().get_layer_surface(&surface, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, "rust-slurp".to_string(), qh, ());
                    layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right | zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left);
                    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
                    layer_surface.set_exclusive_zone(-1);
                    surface.commit();

                    let xdg_output = state.xdg_output_manager.as_ref().unwrap().get_xdg_output(&output, qh, ());

                    state.outputs.push(OutputState {
                        output,
                        xdg_output,
                        logical_pos: (0, 0),
                        size: (0, 0),
                        surface,
                        layer_surface,
                        buffer: None,
                    });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State { fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<wl_shm::WlShm, ()> for State { fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for State { fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: zwlr_layer_shell_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State { fn event(_: &mut Self, _: &zxdg_output_manager_v1::ZxdgOutputManagerV1, _: zxdg_output_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
            if key == 1 && key_state == WEnum::Value(wl_keyboard::KeyState::Pressed) { // Escape key
                state.running = false;
                state.exit_code = 1;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                if let Some(index) = state.outputs.iter().position(|o| o.surface.id() == surface.id()) {
                    state.current_output = Some(index);
                    let output = &state.outputs[index];
                    let (ox, oy) = output.logical_pos;
                    state.current_pos = (ox as f64 + surface_x, oy as f64 + surface_y);
                    state.draw();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.current_output = None;
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some(output_idx) = state.current_output {
                    if let Some(output) = state.outputs.get(output_idx) {
                         let (ox, oy) = output.logical_pos;
                         state.current_pos = (ox as f64 + surface_x, oy as f64 + surface_y);
                         if state.start_pos.is_some() {
                             state.draw();
                         }
                    }
                }
            }
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                match button {
                    272 => { // Left mouse button
                        if btn_state == WEnum::Value(wl_pointer::ButtonState::Pressed) {
                            state.start_pos = Some(state.current_pos);
                        } else { // Released
                            if let Some(start) = state.start_pos.take() {
                                let selection = get_selection_box(start, state.current_pos);
                                if selection.2 > 1.0 && selection.3 > 1.0 {
                                    println!("{},{} {}x{}", selection.0 as i32, selection.1 as i32, selection.2 as i32, selection.3 as i32);
                                    state.exit_code = 0;
                                } else {
                                    // Selection was just a click or too small, count as cancellation
                                    state.exit_code = 1;
                                }
                                state.running = false;
                            }
                        }
                    }
                    273 => { // Right mouse button now acts as cancel
                        state.running = false;
                        state.exit_code = 1;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State { fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surface.ack_configure(serial);
                if let Some(output_index) = state.outputs.iter().position(|o| o.layer_surface.id() == surface.id()) {
                    let output_state = &mut state.outputs[output_index];
                    if output_state.buffer.is_some() && output_state.buffer.as_ref().unwrap().width == width as i32 && output_state.buffer.as_ref().unwrap().height == height as i32 {
                        state.draw_on_output(output_index);
                        return;
                    }

                    let file = tempfile::tempfile().unwrap();
                    let stride = cairo::Format::ARgb32.stride_for_width(width).unwrap();
                    let size = (stride * height as i32) as i32;
                    file.set_len(size as u64).unwrap();

                    let pool = state.shm.as_ref().unwrap().create_pool(unsafe { BorrowedFd::borrow_raw(file.as_raw_fd()) }, size, qh, ());
                    let mmap = unsafe { MmapMut::map_mut(&file).unwrap() };

                    output_state.buffer = Some(Buffer { pool, width: width as i32, height: height as i32, _file: file, mmap });
                    state.draw_on_output(output_index);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.running = false;
                state.exit_code = 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(state: &mut Self, output: &WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|o| o.output.id() == output.id()) {
                entry.size = (width as u32, height as u32);
            }
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        xdg_output: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(output_state) = state.outputs.iter_mut().find(|o| o.xdg_output.id() == xdg_output.id()) {
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    output_state.logical_pos = (x, y);
                }
                zxdg_output_v1::Event::LogicalSize { .. } => {}
                zxdg_output_v1::Event::Done => {}
                zxdg_output_v1::Event::Name { .. } => {}
                zxdg_output_v1::Event::Description { .. } => {}
                _ => {}
            }
        }
    }
}

impl Dispatch<WlShmPool, ()> for State { fn event(_: &mut Self, _: &WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for State { fn event(_: &mut Self, _: &wayland_client::protocol::wl_buffer::WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }