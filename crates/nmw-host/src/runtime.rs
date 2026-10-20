use crate::{HostEvent, Mesh};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const STEP: Duration = Duration::from_millis(20);

/// Drives a [`Mesh`] in real time on a background thread.
pub struct Runtime {
    mesh: Arc<Mutex<Box<dyn Mesh>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Runtime {
    /// `speed` scales mesh time against wall-clock time (1.0 = real time).
    pub fn start(
        mesh: Box<dyn Mesh>,
        speed: f64,
        on_event: impl Fn(HostEvent) + Send + 'static,
    ) -> Runtime {
        let mesh = Arc::new(Mutex::new(mesh));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let mesh = mesh.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let origin = mesh.lock().expect("mesh lock").now();
                while !stop.load(Ordering::Relaxed) {
                    let target = origin + (started.elapsed().as_secs_f64() * speed * 1e6) as u64;
                    let events = {
                        let mut m = mesh.lock().expect("mesh lock");
                        m.advance_to(target);
                        m.drain_events()
                    };
                    for e in events {
                        on_event(e);
                    }
                    std::thread::sleep(STEP);
                }
            })
        };
        Runtime {
            mesh,
            stop,
            thread: Some(thread),
        }
    }

    /// Run `f` with exclusive access to the mesh.
    pub fn with<R>(&self, f: impl FnOnce(&mut dyn Mesh) -> R) -> R {
        let mut m = self.mesh.lock().expect("mesh lock");
        f(m.as_mut())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
