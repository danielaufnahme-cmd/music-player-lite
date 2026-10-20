//! Offscreen platform for screenshots: no window, no compositor. A custom
//! `slint::platform::Platform` with a `MinimalSoftwareWindow` renders with the
//! software renderer into a pixel buffer that is saved as PNG.
//!
//! `geonix-lite --screenshot <screen|all> <out.png|dir> [--size 1700x950]`

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{EventLoopProxy, Platform, WindowAdapter};
use slint::{EventLoopError, PlatformError};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    quit: bool,
}

#[derive(Clone, Default)]
struct Shared(Arc<(Mutex<Queue>, Condvar)>);

struct Proxy(Shared);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        let (m, c) = &*self.0 .0;
        m.lock().unwrap().quit = true;
        c.notify_all();
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Box<dyn FnOnce() + Send>) -> Result<(), EventLoopError> {
        let (m, c) = &*self.0 .0;
        m.lock().unwrap().jobs.push_back(event);
        c.notify_all();
        Ok(())
    }
}

pub struct Offscreen {
    window: Rc<MinimalSoftwareWindow>,
    /// Render every frame like a real window would (benchmarks).
    live: bool,
    shared: Shared,
    start: Instant,
}

thread_local! {
    static LIVE_BUF: std::cell::RefCell<Vec<PremultipliedRgbaColor>> = const { std::cell::RefCell::new(Vec::new()) };
    static WINDOW: std::cell::RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { std::cell::RefCell::new(None) };
}

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.shared.clone())))
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let (m, c) = &*self.shared.0;
        loop {
            slint::platform::update_timers_and_animations();
            let jobs: Vec<Job> = {
                let mut q = m.lock().unwrap();
                if q.quit {
                    q.quit = false;
                    return Ok(());
                }
                q.jobs.drain(..).collect()
            };
            let had_jobs = !jobs.is_empty();
            for j in jobs {
                j();
            }
            if had_jobs {
                continue;
            }
            if self.live {
                LIVE_BUF.with(|b| {
                    let mut b = b.borrow_mut();
                    let size = self.window.size();
                    let w = size.width as usize;
                    b.resize(w * size.height as usize, PremultipliedRgbaColor::default());
                    self.window.draw_if_needed(|r| {
                        r.render(&mut b, w);
                    });
                });
            }
            // Nothing is drawn until a screenshot is taken; just wait for the
            // next timer or job.
            let wait =
                slint::platform::duration_until_next_timer_update().unwrap_or(Duration::from_millis(200)).min(Duration::from_millis(200));
            let q = m.lock().unwrap();
            if q.jobs.is_empty() && !q.quit {
                let _ = c.wait_timeout(q, wait).unwrap();
            }
        }
    }
}

/// Install the offscreen platform with a window of `w`×`h` logical pixels.
pub fn install(w: u32, h: u32, live: bool) {
    // Live mode reuses one buffer, so only dirty regions are repainted (like a real window).
    let window = MinimalSoftwareWindow::new(if live { RepaintBufferType::ReusedBuffer } else { RepaintBufferType::NewBuffer });
    window.set_size(slint::PhysicalSize::new(w, h));
    WINDOW.with(|x| *x.borrow_mut() = Some(window.clone()));
    slint::platform::set_platform(Box::new(Offscreen { window, live, shared: Shared::default(), start: Instant::now() }))
        .expect("offscreen platform");
}

pub fn active() -> bool {
    WINDOW.with(|x| x.borrow().is_some())
}

/// Render the whole window now and return RGBA8 pixels.
pub fn capture() -> Option<(Vec<u8>, u32, u32)> {
    WINDOW.with(|x| {
        let x = x.borrow();
        let window = x.as_ref()?;
        let size = window.size();
        let (w, h) = (size.width as usize, size.height as usize);
        let mut buf = vec![PremultipliedRgbaColor::default(); w * h];
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buf, w);
        });
        let mut out = Vec::with_capacity(w * h * 4);
        for p in buf {
            // opaque window background: un-premultiply is a no-op for alpha 255
            let a = p.alpha.max(1) as u32;
            let un = |c: u8| ((c as u32 * 255 + a / 2) / a).min(255) as u8;
            out.extend_from_slice(&[un(p.red), un(p.green), un(p.blue), 255]);
        }
        Some((out, w as u32, h as u32))
    })
}

pub fn save_png(path: &std::path::Path) -> Result<(), String> {
    let (px, w, h) = capture().ok_or("no offscreen window")?;
    image::save_buffer(path, &px, w, h, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())
}
