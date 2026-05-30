use std::collections::HashSet;
use std::future::poll_fn;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::task::Poll;

use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use evdev::uinput::VirtualDevice;
use tokio::io::unix::AsyncFd;

use styx_proto::Event;

/// A single grabbed keyboard node plus the readiness fd tokio polls.
/// `device` reads the original fd; `async_fd` is a nonblocking dup used
/// only as a readiness signal (the dup shares the open file description,
/// so draining `device` clears readability on both).
struct KbDevice {
    device: Device,
    async_fd: AsyncFd<std::os::fd::OwnedFd>,
    path: PathBuf,
}

impl KbDevice {
    fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let device = Device::open(path)?;
        let async_fd = AsyncFd::new(dup_fd_nonblock(device.as_raw_fd())?)?;
        log::info!(
            "opened evdev device: {} ({})",
            device.name().unwrap_or("unknown"),
            path.display()
        );
        Ok(KbDevice { device, async_fd, path: path.to_path_buf() })
    }
}

/// Result of draining one device on a single wakeup.
enum DrainResult {
    Events(Vec<Event>),
    Empty,
    Gone,
}

/// What woke `next_events`.
enum ReadyKind {
    Device(usize),
    Inotify,
}

pub struct EvdevCapture {
    devices: Vec<KbDevice>,
    synth: VirtualDevice,
    held_keys: HashSet<u32>,
    keys_at_grab: HashSet<u32>,
    grabbed: bool,
    // inotify on /dev/input/by-id so a keyboard hot-plugged (or a wireless
    // dongle switched to USB, which re-enumerates as a *new* node) mid-session
    // gets grabbed without restarting the sender. `None` if the watch could
    // not be set up.
    inotify: Option<AsyncFd<std::os::fd::OwnedFd>>,
    // Super+M is the local Hyprland binding for `styx-toggle`. While the
    // sender holds an exclusive evdev grab the compositor never sees it,
    // so we intercept it here: the M press/release/repeat is swallowed
    // (never forwarded to the receiver) and a one-shot signal is raised
    // for main to release the grab and re-run styx-toggle.
    escape_armed: bool,
    escape_signal: bool,
}

impl EvdevCapture {
    /// Open every keyboard node. `paths` is typically the result of
    /// [`enumerate_keyboards`]; a single explicit config path also works.
    pub fn open(paths: &[PathBuf]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut devices = Vec::new();
        for path in paths {
            match KbDevice::open(path) {
                Ok(d) => devices.push(d),
                Err(e) => log::warn!("failed to open keyboard {}: {e}", path.display()),
            }
        }
        if devices.is_empty() {
            return Err("no keyboard devices could be opened".into());
        }

        // The synthetic device used to inject release events on ungrab must
        // advertise every key any real keyboard can emit, so union their
        // supported-key sets.
        let mut keys = AttributeSet::<KeyCode>::new();
        for d in &devices {
            if let Some(supported) = d.device.supported_keys() {
                for key in supported.iter() {
                    keys.insert(key);
                }
            }
        }
        let synth = VirtualDevice::builder()?
            .name("styx-synth")
            .with_keys(&keys)?
            .build()?;

        Ok(EvdevCapture {
            devices,
            synth,
            held_keys: HashSet::new(),
            keys_at_grab: HashSet::new(),
            grabbed: false,
            inotify: setup_inotify(),
            escape_armed: false,
            escape_signal: false,
        })
    }

    pub fn grab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Re-scan first so a keyboard switched while we were idle (not
        // grabbed) is picked up at the next capture, even without inotify.
        self.rescan();

        if !self.grabbed {
            self.keys_at_grab.clear();
            for d in &self.devices {
                if let Ok(state) = d.device.get_key_state() {
                    self.keys_at_grab.extend(state.iter().map(|k| k.code() as u32));
                }
            }
            let mut grabbed_any = false;
            for d in &mut self.devices {
                match d.device.grab() {
                    Ok(()) => grabbed_any = true,
                    Err(e) => log::warn!("grab failed for {}: {e}", d.path.display()),
                }
            }
            if !grabbed_any {
                return Err("failed to grab any keyboard device".into());
            }
            self.grabbed = true;
            self.escape_armed = false;
            self.escape_signal = false;
            log::debug!(
                "evdev grab acquired ({} devices, {} keys held)",
                self.devices.len(),
                self.keys_at_grab.len()
            );
        }
        Ok(())
    }

    pub fn ungrab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.grabbed {
            for d in &mut self.devices {
                let _ = d.device.ungrab();
            }
            self.grabbed = false;
            self.escape_armed = false;

            // Keys the compositor saw go down before the grab but that were
            // released while grabbed need synthetic releases injected via
            // uinput, otherwise the compositor considers them stuck. A code
            // counts as still held only if some device still reports it down.
            let mut released = 0u32;
            for &code in &self.keys_at_grab {
                let still_down = self.devices.iter().any(|d| {
                    d.device
                        .get_key_state()
                        .map(|s| s.contains(KeyCode(code as u16)))
                        .unwrap_or(false)
                });
                if !still_down {
                    let ev = InputEvent::new(EventType::KEY.0, code as u16, 0);
                    let _ = self.synth.emit(&[ev]);
                    released += 1;
                }
            }
            self.keys_at_grab.clear();
            if released > 0 {
                log::debug!("injected {released} synthetic key releases");
            }
            log::debug!("evdev grab released");
        }
        Ok(())
    }

    pub fn held_modifiers(&self) -> Vec<u32> {
        let mut held = HashSet::new();
        for d in &self.devices {
            let Ok(state) = d.device.get_key_state() else { continue };
            for &code in styx_keymap::MODIFIER_KEYS {
                if state.contains(KeyCode(code as u16)) {
                    held.insert(code);
                }
            }
        }
        held.into_iter().collect()
    }

    pub fn release_all(&mut self) -> Vec<Event> {
        let events: Vec<Event> = self
            .held_keys
            .iter()
            .map(|&code| Event::KeyRelease { code })
            .collect();
        self.held_keys.clear();
        events
    }

    /// One-shot: returns true if a Super+M chord was observed since the
    /// last call. The chord's M events are suppressed from `next_events`
    /// regardless; this signal tells main to release the grab and re-run
    /// the user's styx-toggle binding.
    pub fn take_escape_signal(&mut self) -> bool {
        let s = self.escape_signal;
        self.escape_signal = false;
        s
    }

    /// Re-enumerate keyboards and open (and grab, if currently grabbed) any
    /// node not already tracked. Cheap; safe to call often.
    pub fn rescan(&mut self) {
        let known: HashSet<PathBuf> = self.devices.iter().map(|d| d.path.clone()).collect();
        for path in enumerate_keyboards() {
            if known.contains(&path) {
                continue;
            }
            match KbDevice::open(&path) {
                Ok(mut d) => {
                    if self.grabbed {
                        if let Err(e) = d.device.grab() {
                            log::warn!("grab failed for new keyboard {}: {e}", path.display());
                        }
                    }
                    log::info!("keyboard added: {}", path.display());
                    self.devices.push(d);
                }
                // by-id symlinks can appear a beat before the node is
                // openable; the next rescan/inotify event retries.
                Err(e) => log::debug!("new keyboard {} not yet openable: {e}", path.display()),
            }
        }
    }

    /// Await readiness on any device fd (or inotify), drain it, and return
    /// the resulting events. Returns `None` only when every keyboard device
    /// has gone away. Empty drains loop internally until real events arrive
    /// or a device is lost, so an empty vec is never returned.
    pub async fn next_events(&mut self) -> Option<Vec<Event>> {
        loop {
            match self.wait_ready().await {
                ReadyKind::Device(i) => match self.drain_device(i).await {
                    DrainResult::Events(events) => return Some(events),
                    DrainResult::Empty => continue,
                    DrainResult::Gone => {
                        let dev = self.devices.remove(i);
                        log::warn!("keyboard removed: {}", dev.path.display());
                        if self.devices.is_empty() {
                            return None;
                        }
                    }
                },
                ReadyKind::Inotify => {
                    self.drain_inotify().await;
                    self.rescan();
                }
            }
        }
    }

    /// Poll every device fd and the inotify fd; return the first ready one.
    async fn wait_ready(&self) -> ReadyKind {
        poll_fn(|cx| {
            for (i, d) in self.devices.iter().enumerate() {
                if let Poll::Ready(Ok(_guard)) = d.async_fd.poll_read_ready(cx) {
                    // Drop the guard without clearing: readiness is retained,
                    // so drain_device re-acquires instantly and clears it
                    // properly once the fd drains to EAGAIN.
                    return Poll::Ready(ReadyKind::Device(i));
                }
            }
            if let Some(ino) = &self.inotify {
                if let Poll::Ready(Ok(_guard)) = ino.poll_read_ready(cx) {
                    return Poll::Ready(ReadyKind::Inotify);
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Fully drain one device, translating raw events into protocol events.
    async fn drain_device(&mut self, i: usize) -> DrainResult {
        // Collect raw events while holding the readiness guard, then drop the
        // device borrow before translating (which needs `&mut self`).
        let path = self.devices[i].path.display().to_string();
        let mut raw_events: Vec<InputEvent> = Vec::new();
        {
            let dev = &mut self.devices[i];
            let mut guard = match dev.async_fd.readable().await {
                Ok(g) => g,
                Err(_) => return DrainResult::Gone,
            };
            loop {
                match dev.device.fetch_events() {
                    Ok(events) => raw_events.extend(events),
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
                        guard.clear_ready();
                        break;
                    }
                    Err(e) => {
                        log::warn!("evdev read failed on {path}: {e}");
                        return DrainResult::Gone;
                    }
                }
            }
        }

        let mut out = Vec::new();
        for ev in raw_events {
            self.translate(ev, &mut out);
        }

        if out.is_empty() {
            DrainResult::Empty
        } else {
            DrainResult::Events(out)
        }
    }

    /// Translate one raw input event into zero or more protocol events,
    /// applying held-key tracking and the Super+M escape interception.
    fn translate(&mut self, ev: InputEvent, out: &mut Vec<Event>) {
        let summary: EventSummary = ev.into();
        let EventSummary::Key(_key_ev, key_code, value) = summary else {
            return;
        };
        let code = key_code.0 as u32;
        match value {
            1 => {
                let super_held = self.held_keys.contains(&styx_keymap::KEY_LEFT_META)
                    || self.held_keys.contains(&styx_keymap::KEY_RIGHT_META);
                if code == styx_keymap::KEY_M && super_held {
                    self.escape_armed = true;
                    self.escape_signal = true;
                    return;
                }
                self.held_keys.insert(code);
                out.push(Event::KeyPress { code });
            }
            0 => {
                if code == styx_keymap::KEY_M && self.escape_armed {
                    self.escape_armed = false;
                    return;
                }
                self.held_keys.remove(&code);
                out.push(Event::KeyRelease { code });
            }
            2 => {
                // Kernel auto-repeat. Forward as another key press
                // since macOS doesn't repeat programmatically posted events.
                // Suppress repeats for modifier keys -- they cause
                // duplicate modifier-down events on macOS which triggers
                // unintended shortcuts and special characters.
                if code == styx_keymap::KEY_M && self.escape_armed {
                    return;
                }
                if !styx_keymap::is_modifier(code) {
                    out.push(Event::KeyPress { code });
                }
            }
            _ => {}
        }
    }

    /// Drain pending inotify records. Contents are ignored -- any event just
    /// triggers a rescan. `try_io` clears tokio's readiness once the read
    /// returns EAGAIN, so the fd re-arms for the next hot-plug.
    async fn drain_inotify(&mut self) {
        let Some(ino) = &self.inotify else { return };
        loop {
            let Ok(guard) = ino.readable().await else { return };
            let mut guard = guard;
            let res = guard.try_io(|inner| {
                let raw = inner.get_ref().as_raw_fd();
                let mut buf = [0u8; 4096];
                let n =
                    unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n)
                }
            });
            match res {
                Ok(Ok(n)) if n > 0 => continue,
                _ => return,
            }
        }
    }
}

/// Scan /dev/input/by-id for keyboard event nodes. Returns all matches,
/// sorted, so the set is deterministic across calls.
pub fn enumerate_keyboards() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev/input/by-id/") else {
        return Vec::new();
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.contains("kbd") && name.contains("event") && !name.contains("if0")
        })
        .collect();
    candidates.sort();
    candidates
}

/// Create an inotify fd watching /dev/input/by-id for new nodes, wrapped in
/// an `AsyncFd`. Returns `None` (logging a warning) if it cannot be set up;
/// the sender then falls back to rescanning on each grab.
fn setup_inotify() -> Option<AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        log::warn!("inotify_init1 failed; keyboard hot-plug disabled");
        return None;
    }
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let path = c"/dev/input/by-id";
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            path.as_ptr(),
            libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_TO,
        )
    };
    if wd < 0 {
        log::warn!("inotify watch on /dev/input/by-id failed; keyboard hot-plug disabled");
        return None;
    }
    match AsyncFd::new(owned) {
        Ok(afd) => Some(afd),
        Err(e) => {
            log::warn!("AsyncFd for inotify failed: {e}; keyboard hot-plug disabled");
            None
        }
    }
}

fn dup_fd_nonblock(raw: std::os::fd::RawFd) -> Result<std::os::fd::OwnedFd, std::io::Error> {
    use std::os::fd::FromRawFd;
    let new_fd = unsafe { libc::dup(raw) };
    if new_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(new_fd, libc::F_GETFL) };
    unsafe { libc::fcntl(new_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) })
}
