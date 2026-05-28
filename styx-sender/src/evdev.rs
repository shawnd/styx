use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::Path;

use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use evdev::uinput::VirtualDevice;
use tokio::io::unix::AsyncFd;

use styx_proto::Event;

pub struct EvdevCapture {
    device: Device,
    synth: VirtualDevice,
    held_keys: HashSet<u32>,
    keys_at_grab: HashSet<u32>,
    grabbed: bool,
    // Super+M is the local Hyprland binding for `styx-toggle`. While the
    // sender holds an exclusive evdev grab the compositor never sees it,
    // so we intercept it here: the M press/release/repeat is swallowed
    // (never forwarded to the receiver) and a one-shot signal is raised
    // for main to release the grab and re-run styx-toggle.
    escape_armed: bool,
    escape_signal: bool,
}

impl EvdevCapture {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let device = Device::open(path)?;
        log::info!(
            "opened evdev device: {} ({})",
            device.name().unwrap_or("unknown"),
            path.display()
        );

        let mut keys = AttributeSet::<KeyCode>::new();
        if let Some(supported) = device.supported_keys() {
            for key in supported.iter() {
                keys.insert(key);
            }
        }
        let synth = VirtualDevice::builder()?
            .name("styx-synth")
            .with_keys(&keys)?
            .build()?;

        Ok(EvdevCapture {
            device,
            synth,
            held_keys: HashSet::new(),
            keys_at_grab: HashSet::new(),
            grabbed: false,
            escape_armed: false,
            escape_signal: false,
        })
    }

    pub fn grab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.grabbed {
            if let Ok(state) = self.device.get_key_state() {
                self.keys_at_grab = state.iter().map(|k| k.code() as u32).collect();
            }
            self.device.grab()?;
            self.grabbed = true;
            self.escape_armed = false;
            self.escape_signal = false;
            log::debug!("evdev grab acquired ({} keys held)", self.keys_at_grab.len());
        }
        Ok(())
    }

    pub fn ungrab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.grabbed {
            self.device.ungrab()?;
            self.grabbed = false;
            self.escape_armed = false;

            // Keys the compositor saw go down before the grab but that were
            // released while grabbed need synthetic releases injected via
            // uinput, otherwise the compositor considers them stuck.
            let current = self.device.get_key_state().unwrap_or_default();
            let mut released = 0u32;
            for &code in &self.keys_at_grab {
                if !current.contains(KeyCode(code as u16)) {
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
        let Ok(state) = self.device.get_key_state() else {
            return vec![];
        };
        styx_keymap::MODIFIER_KEYS
            .iter()
            .copied()
            .filter(|&code| state.contains(KeyCode(code as u16)))
            .collect()
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
    /// last call. The chord's M events are suppressed from `read_events`
    /// regardless; this signal tells main to release the grab and re-run
    /// the user's styx-toggle binding.
    pub fn take_escape_signal(&mut self) -> bool {
        let s = self.escape_signal;
        self.escape_signal = false;
        s
    }

    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.device.as_raw_fd()
    }

    /// Returns `Some(events)` on success, `None` if the device is gone.
    pub fn read_events(&mut self) -> Option<Vec<Event>> {
        let events = match self.device.fetch_events() {
            Ok(events) => events,
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Some(vec![]),
            Err(e) => {
                log::warn!("evdev read failed: {e}");
                return None;
            }
        };

        let mut out = Vec::new();
        for ev in events {
            let summary: EventSummary = ev.into();
            if let EventSummary::Key(_key_ev, key_code, value) = summary {
                let code = key_code.0 as u32;
                match value {
                    1 => {
                        let super_held = self.held_keys.contains(&styx_keymap::KEY_LEFT_META)
                            || self.held_keys.contains(&styx_keymap::KEY_RIGHT_META);
                        if code == styx_keymap::KEY_M && super_held {
                            self.escape_armed = true;
                            self.escape_signal = true;
                            continue;
                        }
                        self.held_keys.insert(code);
                        out.push(Event::KeyPress { code });
                    }
                    0 => {
                        if code == styx_keymap::KEY_M && self.escape_armed {
                            self.escape_armed = false;
                            continue;
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
                            continue;
                        }
                        if !styx_keymap::is_modifier(code) {
                            out.push(Event::KeyPress { code });
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(out)
    }
}

pub struct AsyncEvdev {
    fd: AsyncFd<std::os::fd::OwnedFd>,
}

impl AsyncEvdev {
    pub fn new(capture: &EvdevCapture) -> Result<Self, std::io::Error> {
        let duped = dup_fd_nonblock(capture.raw_fd())?;
        Ok(AsyncEvdev {
            fd: AsyncFd::new(duped)?,
        })
    }

    pub async fn readable(&self) -> Result<(), std::io::Error> {
        let mut guard = self.fd.readable().await?;
        guard.retain_ready();
        Ok(())
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
