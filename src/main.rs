use std::process::exit;
use std::os::unix::io::{AsRawFd, BorrowedFd};

use cairo::{Context, Format, ImageSurface};
use memmap2::MmapMut;

use wayland_client::protocol::{
    wl_compositor,
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut state = State {
        running: true,
        qh: qh.clone(),
        compositor: None,
        shm: None,
        layer_shell: None,
        seat: None,
        pointer: None,
        layer_surface: None,
        surface: None,
        outputs: Vec::new(),
        buffer: None,
        start_pos: None,
        current_pos: (0.0, 0.0),
    };

    event_queue.roundtrip(&mut state).unwrap();

    if state.compositor.is_none() || state.shm.is_none() || state.layer_shell.is_none() || state.seat.is_none() {
        eprintln!("Error: Your compositor does not support the required Wayland protocols.");
        exit(1);
    }

    state.create_surface();
    state.create_layer_surface();

    event_queue.roundtrip(&mut state).unwrap();

    while state.running {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }
}

struct State {
    running: bool,
    qh: QueueHandle<Self>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    surface: Option<wl_surface::WlSurface>,
    outputs: Vec<(WlOutput, (u32, u32))>, // output and its size
    buffer: Option<Buffer>,
    start_pos: Option<(f64, f64)>,
    current_pos: (f64, f64),
}

struct Buffer {
    pool: WlShmPool,
    width: i32,
    height: i32,
    _file: std::fs::File,
    mmap: MmapMut,
}

impl State {
    fn create_surface(&mut self) {
        let surface = self.compositor.as_ref().unwrap().create_surface(&self.qh, ());
        self.surface = Some(surface);
    }

    fn create_layer_surface(&mut self) {
        let layer_shell = self.layer_shell.as_ref().unwrap();
        let surface = self.surface.as_ref().unwrap();

        let layer_surface = layer_shell.get_layer_surface(surface, None, zwlr_layer_shell_v1::Layer::Overlay, "rust-slurp".to_string(), &self.qh, ());
        layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right | zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left);
        layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
        layer_surface.set_exclusive_zone(-1);
        surface.commit();

        self.layer_surface = Some(layer_surface);
    }

    fn draw(&mut self) {
        if let Some(buffer) = self.buffer.as_mut() {
            let width = buffer.width;
            let height = buffer.height;
            let stride = cairo::Format::ARgb32.stride_for_width(width as u32).unwrap();

            let wl_surface = self.surface.as_ref().unwrap();
            let wl_buffer = buffer.pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &self.qh, ());

            {
                let mmap = &mut buffer.mmap[..];
                let surface = unsafe { ImageSurface::create_for_data_unsafe(mmap.as_mut_ptr(), Format::ARgb32, width, height, stride).unwrap() };
                let ctx = Context::new(&surface).unwrap();

                // Draw semi-transparent background
                ctx.set_source_rgba(0.0, 0.0, 0.0, 0.2);
                ctx.set_operator(cairo::Operator::Source);
                ctx.paint().unwrap();

                if let Some(start) = self.start_pos {
                    let (x, y, w, h) = self.get_selection_box(start, self.current_pos);
                    
                    // Clear the selection area
                    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.0);
                    ctx.rectangle(x, y, w, h);
                    ctx.fill().unwrap();

                    // Draw selection border
                    ctx.set_source_rgba(0.2, 0.6, 1.0, 0.8);
                    ctx.set_line_width(2.0);
                    ctx.rectangle(x, y, w, h);
                    ctx.stroke().unwrap();
                }

                surface.flush();
            }

            wl_surface.attach(Some(&wl_buffer), 0, 0);
            wl_surface.damage_buffer(0, 0, width, height);
            wl_surface.commit();
            wl_buffer.destroy();
        }
    }

    fn get_selection_box(&self, p1: (f64, f64), p2: (f64, f64)) -> (f64, f64, f64, f64) {
        let x = p1.0.min(p2.0);
        let y = p1.1.min(p2.1);
        let w = (p1.0 - p2.0).abs();
        let h = (p1.1 - p2.1).abs();
        (x, y, w, h)
    }
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
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    state.pointer = Some(seat.get_pointer(qh, ()));
                    state.seat = Some(seat);
                }
                "wl_output" => {
                    let output: WlOutput = registry.bind(name, version, qh, ());
                    state.outputs.push((output, (0, 0)));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _: &mut Self, 
        _: &wl_compositor::WlCompositor, 
        _: wl_compositor::Event, 
        _: &(), 
        _: &Connection, 
        _: &QueueHandle<Self>
    ) {}
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: zwlr_layer_shell_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_seat::Event::Name { name } = event {
            eprintln!("Seat name: {}", name);
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { .. } => {}
            wl_pointer::Event::Leave { .. } => {}
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.current_pos = (surface_x, surface_y);
                if state.start_pos.is_some() {
                    state.draw();
                }
            }
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                if button == 272 { // Left mouse button
                    if btn_state == WEnum::Value(wl_pointer::ButtonState::Pressed) {
                        state.start_pos = Some(state.current_pos);
                    } else {
                        if let Some(start) = state.start_pos {
                            let (x, y, w, h) = state.get_selection_box(start, state.current_pos);
                            println!("{},{} {}x{}", x as i32, y as i32, w as i32, h as i32);
                            state.running = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

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
                if state.buffer.is_some() && state.buffer.as_ref().unwrap().width == width as i32 && state.buffer.as_ref().unwrap().height == height as i32 {
                    state.draw();
                    return;
                }

                let file = tempfile::tempfile().unwrap();
                let stride = cairo::Format::ARgb32.stride_for_width(width).unwrap();
                let size = (stride * height as i32) as i32;
                file.set_len(size as u64).unwrap();

                let pool = state.shm.as_ref().unwrap().create_pool(unsafe { BorrowedFd::borrow_raw(file.as_raw_fd()) }, size, qh, ());
                let mmap = unsafe { MmapMut::map_mut(&file).unwrap() };

                state.buffer = Some(Buffer { pool, width: width as i32, height: height as i32, _file: file, mmap });
                state.draw();
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(state: &mut Self, output: &WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|(o, _)| o == output) {
                entry.1 = (width as u32, height as u32);
            }
        }
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_buffer::WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
