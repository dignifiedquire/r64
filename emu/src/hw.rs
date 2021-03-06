extern crate byteorder;
extern crate sdl2;

use self::sdl2::event::Event;
use self::sdl2::keyboard::Keycode;
use self::sdl2::pixels::PixelFormatEnum;
use self::sdl2::render::{TextureCreator, WindowCanvas};
use self::sdl2::video::WindowContext;
use super::gfx::{GfxBufferLE, GfxBufferMutLE, OwnedGfxBufferLE, Rgb888};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

pub struct OutputConfig {
    pub window_title: String,
    pub width: isize,
    pub height: isize,
    pub fps: isize,
    pub enforce_speed: bool,
}

struct Video {
    canvas: WindowCanvas,
    creator: TextureCreator<WindowContext>,

    cfg: Rc<OutputConfig>,
    fps_clock: SystemTime,
    fps_counter: isize,
}

impl Video {
    fn new(cfg: Rc<OutputConfig>, context: &sdl2::Sdl) -> Result<Video, String> {
        let sub = context
            .video()
            .or_else(|e| Err(format!("error creating video subsystem: {:?}", e)))?;
        let window = sub
            .window(&cfg.window_title, 800, 600)
            .resizable()
            .position_centered()
            .opengl()
            .build()
            .or_else(|e| Err(format!("error creating window: {:?}", e)))?;
        let mut canvas = window
            .into_canvas()
            .software()
            .build()
            .or_else(|e| Err(format!("error creating canvas: {:?}", e)))?;
        let creator = canvas.texture_creator();

        canvas.set_logical_size(cfg.width as u32, cfg.height as u32);

        Ok(Video {
            cfg,
            canvas,
            creator,
            fps_clock: SystemTime::now(),
            fps_counter: 0,
        })
    }

    fn render_frame(&mut self, frame: &GfxBufferLE<Rgb888>) {
        self.draw(frame);
        self.update_fps();
    }

    fn draw(&mut self, frame: &GfxBufferLE<Rgb888>) {
        let mut tex = self
            .creator
            .create_texture_target(
                PixelFormatEnum::ABGR8888,
                self.cfg.width as u32,
                self.cfg.height as u32,
            )
            .unwrap();
        let (mem, pitch) = frame.raw();
        tex.update(None, mem, pitch);
        self.canvas.copy(&tex, None, None);
        self.canvas.present();
    }

    fn update_fps(&mut self) {
        self.fps_counter += 1;
        let one_second = Duration::new(1, 0);
        match self.fps_clock.elapsed() {
            Ok(elapsed) if elapsed >= one_second => {
                self.canvas.window_mut().set_title(&format!(
                    "{} - {} FPS",
                    &self.cfg.window_title, self.fps_counter
                ));
                self.fps_counter = 0;
                self.fps_clock += one_second;
            }
            _ => {}
        }
    }
}

pub trait OutputProducer {
    fn render_frame(&mut self, screen: &mut GfxBufferMutLE<Rgb888>);
    fn finish(&mut self);
}

pub struct Output {
    cfg: Rc<OutputConfig>,
    context: sdl2::Sdl,
    video: Option<Video>,
}

impl Output {
    pub fn new(cfg: OutputConfig) -> Result<Output, String> {
        Ok(Output {
            cfg: Rc::new(cfg),
            context: sdl2::init()?,
            video: None,
        })
    }

    pub fn enable_video(&mut self) -> Result<(), String> {
        self.video = Some(Video::new(self.cfg.clone(), &self.context)?);
        Ok(())
    }

    pub fn run<F: 'static + Send + FnOnce() -> Result<Box<OutputProducer>, String>>(
        &mut self,
        create: F,
    ) {
        let width = self.cfg.width as usize;
        let height = self.cfg.height as usize;
        let (tx, rx) = mpsc::sync_channel(3);

        thread::spawn(move || {
            let mut producer = create().unwrap();
            loop {
                let mut screen = OwnedGfxBufferLE::<Rgb888>::new(width, height);
                producer.render_frame(&mut screen.buf_mut());

                tx.send(screen).unwrap();
            }
        });

        loop {
            for event in self.context.event_pump().unwrap().poll_iter() {
                match event {
                    Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    }
                    | Event::Quit { .. } => return,
                    _ => {}
                }
            }

            let screen = rx.recv().unwrap();
            self.render_frame(&screen.buf());
        }
    }

    pub fn render_frame(&mut self, video: &GfxBufferLE<Rgb888>) {
        self.video.as_mut().map(|v| v.render_frame(video));
    }
}
