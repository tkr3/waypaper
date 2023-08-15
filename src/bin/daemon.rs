use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    io::{Seek, Write},
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
    thread,
};

use image::{DynamicImage, GenericImage, GenericImageView};
use log::{debug, error, info, warn};
use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use waypaper::{
    config::{self, Config},
    AppEvent,
};

fn main() {
    env_logger::init();

    let config: Config = Config::search();
    let con = Connection::connect_to_env().unwrap();
    let mut event_queue = con.new_event_queue();
    let qhandle = event_queue.handle();
    let display = con.display();
    display.get_registry(&qhandle, ());
    let (_watcher, rx, tx) = config.watch();
    let state = Arc::new(Mutex::new(State::new(config)));
    let sender = tx.clone();
    thread::spawn({
        let mut signals = signal_hook::iterator::Signals::new(&[libc::SIGUSR1]).unwrap();
        move || {
            for _signal in signals.forever() {
                info!("Received SIGUSR1, reloading config");
                sender.send(AppEvent::ConfigChanged).unwrap();
            }
        }
    });

    thread::spawn({
        let state = Arc::clone(&state);
        move || loop {
            match rx.recv() {
                Ok(event) => {
                    state.lock().unwrap().handle(event).unwrap();
                }
                Err(e) => {
                    error!("Error receiving config event: {}", e);
                }
            }
        }
    });

    thread::spawn({
        let mut dispatcher = Dispatcher {
            state: Arc::clone(&state),
        };
        move || loop {
            event_queue.blocking_dispatch(&mut dispatcher).unwrap();
        }
    })
    .join()
    .unwrap();
}

struct Dispatcher {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct Globals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
}

#[derive(Debug, Default)]
struct State {
    config: config::Config,
    globals: Globals,
    shm_pool: Option<wl_shm_pool::WlShmPool>,
    shm_formats: Vec<wl_shm::Format>,
    buffer_file: Option<File>,
    max_buffer_size: usize,
    buffers: HashMap<String, wl_buffer::WlBuffer>,
    surfaces: HashMap<String, wl_surface::WlSurface>,
    surface_configured: u32,
    outputs: Vec<Output>,
    output_builders: Vec<OutputBuilder>,
    total_pixels: usize,
}

impl State {
    fn new(config: config::Config) -> State {
        State {
            config,
            ..Default::default()
        }
    }

    fn setup_buffer_file(&mut self, qh: &QueueHandle<Dispatcher>) {
        let file = if self.buffer_file.is_some() {
            self.buffer_file.as_ref().unwrap().try_clone().unwrap()
        } else {
            info!("Creating tempfile");
            tempfile::tempfile().expect("Error creating tempfile")
        };

        info!("Resizing tempfile");
        debug!(
            "New buffer size: {}",
            std::cmp::max(self.max_buffer_size, self.total_pixels * 3)
        );

        file.set_len(std::cmp::max(self.max_buffer_size, self.total_pixels * 3) as u64)
            .expect("Error resizing tempfile");

        self.buffer_file = Some(file);

        if let Some(shm_pool) = &self.shm_pool {
            info!("Resizing shm pool");
            shm_pool.resize((self.total_pixels * 3) as i32);
            self.max_buffer_size = std::cmp::max(self.max_buffer_size, self.total_pixels * 3);
        } else {
            info!("Creating shm pool");
            self.shm_pool = Some(self.globals.shm.as_ref().unwrap().create_pool(
                self.buffer_file.as_ref().unwrap().as_raw_fd(),
                (self.total_pixels * 3) as i32,
                qh,
                (),
            ));
        }
    }

    fn setup_buffers(&mut self, qh: &QueueHandle<Dispatcher>) {
        let mut offset = 0;
        for output in self.outputs.clone() {
            self.setup_buffer(&output, offset, qh);
            offset += output.pixel_count * 3;
        }

        info!("Done setting up buffers. Redrawing.");
        self.draw_all().expect("Error drawing to file");
    }

    fn setup_buffer(&mut self, output: &Output, offset: usize, qh: &QueueHandle<Dispatcher>) {
        if self.buffers.contains_key(&output.name) {
            info!("Buffer for output {} already exists", output.name);

            self.buffers.get(&output.name).unwrap().destroy();
        }

        info!("Creating buffer for output: {}", output.name);
        debug!(
            "Buffer size: {}, offset: {}",
            output.pixel_count * 3,
            offset
        );

        let buffer = self.shm_pool.as_ref().unwrap().create_buffer(
            offset as i32,
            output.width as i32,
            output.height as i32,
            output.width as i32 * 3,
            wl_shm::Format::Bgr888,
            qh,
            (),
        );
        let surface = self.surfaces.get(&output.name).unwrap();

        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.commit();

        self.buffers.insert(output.name.to_string(), buffer);
    }

    fn next_output_id(&self) -> usize {
        self.output_builders.len()
    }

    fn draw_all(&self) -> Result<(), Box<dyn Error>> {
        info!("Writing to file");

        debug!("Total pixels: {}", self.total_pixels);

        let mut buf = std::io::BufWriter::new(self.buffer_file.as_ref().unwrap());
        buf.seek(std::io::SeekFrom::Start(0))?;

        // used to check if the buffer position is correct
        let mut buf_pos = 0;

        let output_preferences = self.config.output_preferences.as_ref().unwrap();

        for output in self.outputs.iter() {
            if output_preferences.contains_key(&output.name)
                && output_preferences[&output.name].background.is_some()
            {
                let prefs = &output_preferences[&output.name];
                let background = prefs.background.as_ref().unwrap();
                info!("Loading image: {}", background.display());
                let image = image::io::Reader::open(background)?
                    .with_guessed_format()?
                    .decode()?;

                debug!("Image size: {:?}", image.dimensions());
                info!("{}: Writing background image to buffer", output.name);

                write_image(
                    image,
                    &prefs.mode,
                    output.width as u32,
                    output.height as u32,
                    &mut buf,
                )?;
            } else {
                warn!(
                    "{}: No background image specified, defaulting to black",
                    output.name
                );

                write_default_color(output, &mut buf)?;
                info!("{}: Done writing to buffer", output.name);
            }

            buf_pos += output.pixel_count * 3;
            debug!("Buffer position: {}", buf_pos);
            if let Ok(pos) = buf.seek(std::io::SeekFrom::Current(0)) {
                if pos as usize != buf_pos {
                    warn!(
                        "Buffer position mismatch (real: {}, expected: {})",
                        pos, buf_pos
                    );
                }
            } else {
                warn!("Error getting buffer position");
            }
        }
        std::io::Write::flush(&mut buf).unwrap();
        info!("Done writing to file");

        Ok(())
    }

    fn handle(&mut self, event: AppEvent) -> Result<(), Box<dyn Error>> {
        debug!("Handling event: {:?}", event);

        match event {
            AppEvent::ConfigChanged => {
                self.config.reload()?;
                self.draw_all()?;
                debug!("Damaging surfaces");
                for s in self.surfaces.values() {
                    s.damage_buffer(0, 0, i32::MAX, i32::MAX);
                    s.commit();
                }
                debug!("Done damaging surfaces");
            }
            AppEvent::OutputChanged => {
                info!("Output changed, redrawing");
            }
        }
        Ok(())
    }
}

fn write_image(
    image: DynamicImage,
    mode: &config::Mode,
    width: u32,
    height: u32,
    buf: &mut std::io::BufWriter<&File>,
) -> std::io::Result<()> {
    buf.write_all(
        apply_image_mode(image, mode, width, height)
            .to_rgb8()
            .as_raw(),
    )?;
    Ok(())
}

fn write_default_color(
    output: &Output,
    buf: &mut std::io::BufWriter<&File>,
) -> std::io::Result<()> {
    for _ in 0..output.pixel_count {
        buf.write(&[0, 0, 0])?;
    }
    Ok(())
}

fn apply_image_mode(
    image: DynamicImage,
    mode: &config::Mode,
    target_width: u32,
    target_height: u32,
) -> DynamicImage {
    info!("Applying mode: {}", mode);
    match mode {
        config::Mode::Fill => image.resize_to_fill(
            target_width,
            target_height,
            image::imageops::FilterType::Lanczos3,
        ),
        config::Mode::Center => todo!(),
        config::Mode::Fit => {
            let resized_image = image.resize(
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            );
            let mut result_image = DynamicImage::new_rgba8(target_width, target_height);
            result_image
                .copy_from(
                    &resized_image,
                    (target_width - resized_image.width()) / 2,
                    (target_height - resized_image.height()) / 2,
                )
                .unwrap();
            result_image
        }
        config::Mode::Stretch => image.resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Lanczos3,
        ),
    }
}

#[derive(Debug, Clone)]
struct Output {
    name: String,
    width: usize,
    height: usize,
    pixel_count: usize,
    wl_output: Option<wl_output::WlOutput>,
}

#[derive(Debug, Default, Clone)]
struct OutputBuilder {
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub wl_output: Option<wl_output::WlOutput>,
}

impl OutputBuilder {
    fn build(&self) -> Output {
        Output {
            name: self.name.clone(),
            width: self.width,
            height: self.height,
            pixel_count: self.width * self.height,
            wl_output: self.wl_output.clone(),
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Dispatcher {
    fn event(
        dispatcher: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
            ..
        } = event
        {
            let state = &mut dispatcher.state.lock().unwrap();
            match &interface[..] {
                "wl_compositor" => {
                    state.globals.compositor.replace(
                        registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, qh, ()),
                    );
                }
                "wl_shm" => {
                    state
                        .globals
                        .shm
                        .replace(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                "wl_output" => {
                    registry.bind::<wl_output::WlOutput, usize, _>(
                        name,
                        version,
                        qh,
                        state.next_output_id(),
                    );
                }
                "zwlr_layer_shell_v1" => {
                    state
                        .globals
                        .layer_shell
                        .replace(registry.bind::<ZwlrLayerShellV1, _, _>(name, version, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Dispatcher {
    fn event(
        dispatcher: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width: _,
                height: _,
            } => {
                let state = &mut dispatcher.state.lock().unwrap();
                layer_surface.ack_configure(serial);
                state.surface_configured += 1;

                if state.surfaces.len() == state.surface_configured as usize {
                    state.setup_buffers(qh);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Dispatcher {
    fn event(
        dispatcher: &mut Self,
        wl_output: &wl_output::WlOutput,
        event: wl_output::Event,
        output_id: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let state = &mut dispatcher.state.lock().unwrap();

        let builder = match state.output_builders.get_mut(*output_id) {
            Some(builder) => builder,
            None => {
                state.output_builders.push(OutputBuilder::default());
                state.output_builders.last_mut().unwrap()
            }
        };

        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh: _,
            } => {
                if flags != WEnum::Value(wl_output::Mode::Current) {
                    return;
                }
                // save output mode
                builder.width = width as usize;
                builder.height = height as usize;
            }
            wl_output::Event::Name { name } => {
                // save output name
                builder.name = name;
            }
            wl_output::Event::Done => {
                builder.wl_output = Some(wl_output.clone());

                let builder = state.output_builders.remove(*output_id);
                let output = builder.build();

                state.total_pixels += output.pixel_count;

                let surface = state
                    .globals
                    .compositor
                    .as_ref()
                    .unwrap()
                    .create_surface(qh, ());

                let layer_surface = state
                    .globals
                    .layer_shell
                    .as_ref()
                    .unwrap()
                    .get_layer_surface(
                        &surface,
                        output.wl_output.as_ref(),
                        Layer::Background,
                        String::from("waypaper"),
                        qh,
                        (),
                    );

                layer_surface.set_size(output.width as u32, output.height as u32);
                layer_surface.set_exclusive_zone(-1);

                surface.commit();
                state.surfaces.insert(output.name.clone(), surface);
                state.outputs.push(output);

                state.setup_buffer_file(qh);
            }
            _ => {}
        };
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Dispatcher {
    fn event(
        dispatcher: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event {
            if let WEnum::Value(format) = format {
                dispatcher.state.lock().unwrap().shm_formats.push(format);
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_compositor has no event
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug!("layer shell event: {:?}", event)
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug!("surface event: {:?}", event)
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        event: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug!("shm pool event: {:?}", event)
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug!("buffer event: {:?}", event)
    }
}
