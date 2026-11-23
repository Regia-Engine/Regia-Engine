// SPDX-License-Identifier: MPL-2.0
//! Real key presses into the editor window on the run's private Xvfb, for the keyboard-only pass:
//! a minimal X11 client over the display's Unix socket that sends XTEST `FakeInput` requests (the
//! X11 core protocol and the XTEST extension are public specifications from X.Org). No new
//! dependency; Unix only. With `--windowed` the pass is skipped: it would type into the user's
//! desktop.

use std::io::{Read, Write};

/// X keysyms the pass uses.
pub mod keysym {
    /// Tab.
    pub const TAB: u32 = 0xff09;
    /// Return.
    pub const RETURN: u32 = 0xff0d;
    /// Escape.
    pub const ESCAPE: u32 = 0xff1b;
    /// Down arrow.
    pub const DOWN: u32 = 0xff54;
    /// Up arrow.
    pub const UP: u32 = 0xff52;
    /// Left Control.
    pub const CONTROL_L: u32 = 0xffe3;
    /// Left Shift.
    pub const SHIFT_L: u32 = 0xffe1;
}

/// A top-level window: id, area, x, y, width, height.
type Candidate = (u32, u32, i16, i16, u16, u16);

/// A connection to an X display with the XTEST extension.
pub struct XKeys {
    stream: std::os::unix::net::UnixStream,
    root: u32,
    xtest: u8,
    min_keycode: u8,
    keysyms_per_code: usize,
    map: Vec<u32>,
}

fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

impl XKeys {
    /// Connects to display `:n` (the `DISPLAY` string of the run's Xvfb).
    ///
    /// # Errors
    ///
    /// The socket, the handshake or the XTEST extension is missing.
    pub fn connect(display: &str) -> Result<Self, String> {
        let n = display
            .trim_start_matches(':')
            .split('.')
            .next()
            .and_then(|d| d.parse::<u32>().ok())
            .ok_or_else(|| format!("display {display} is not :<n>"))?;
        let mut stream = std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{n}"))
            .map_err(|e| format!("X socket of {display}: {e}"))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        // Connection setup, little-endian, protocol 11.0, no authorization.
        let mut setup = vec![b'l', 0];
        setup.extend_from_slice(&11u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        stream.write_all(&setup).map_err(|e| e.to_string())?;
        let mut head = [0u8; 8];
        stream.read_exact(&mut head).map_err(|e| e.to_string())?;
        let extra = usize::from(u16_at(&head, 6)) * 4;
        let mut body = vec![0u8; extra];
        stream.read_exact(&mut body).map_err(|e| e.to_string())?;
        if head[0] != 1 {
            return Err(format!(
                "X server refused the connection: {}",
                String::from_utf8_lossy(&body)
            ));
        }
        let vendor_len = usize::from(u16_at(&body, 16));
        let formats = usize::from(body[21]);
        let min_keycode = body[26];
        let max_keycode = body[27];
        let screen = 32 + vendor_len + pad4(vendor_len) + formats * 8;
        if body.len() < screen + 24 {
            return Err("short X setup reply".into());
        }
        let root = u32_at(&body, screen);
        let mut x = Self {
            stream,
            root,
            xtest: 0,
            min_keycode,
            keysyms_per_code: 0,
            map: Vec::new(),
        };
        x.xtest = x.query_extension("XTEST")?;
        x.load_keymap(max_keycode)?;
        Ok(x)
    }

    fn request(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write_all(bytes).map_err(|e| e.to_string())
    }

    /// Reads packets until a reply; an X error is an error.
    fn reply(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let mut head = [0u8; 32];
            self.stream.read_exact(&mut head).map_err(|e| e.to_string())?;
            match head[0] {
                0 => return Err(format!("X error {} on opcode {}", head[1], head[10])),
                1 => {
                    let extra = u32_at(&head, 4) as usize * 4;
                    let mut out = head.to_vec();
                    out.resize(32 + extra, 0);
                    self.stream
                        .read_exact(&mut out[32..])
                        .map_err(|e| e.to_string())?;
                    return Ok(out);
                }
                _ => {} // an event: not selected, but skip it
            }
        }
    }

    fn query_extension(&mut self, name: &str) -> Result<u8, String> {
        let n = name.len();
        let len = 2 + (n + pad4(n)) / 4;
        let mut req = vec![98, 0];
        req.extend_from_slice(&u16::try_from(len).map_err(|e| e.to_string())?.to_le_bytes());
        req.extend_from_slice(&u16::try_from(n).map_err(|e| e.to_string())?.to_le_bytes());
        req.extend_from_slice(&[0, 0]);
        req.extend_from_slice(name.as_bytes());
        req.resize(req.len() + pad4(n), 0);
        self.request(&req)?;
        let r = self.reply()?;
        if r[8] == 0 {
            return Err(format!("the X server has no {name} extension"));
        }
        Ok(r[9])
    }

    fn load_keymap(&mut self, max_keycode: u8) -> Result<(), String> {
        let count = max_keycode.saturating_sub(self.min_keycode).saturating_add(1);
        let req = [101, 0, 2, 0, self.min_keycode, count, 0, 0];
        self.request(&req)?;
        let r = self.reply()?;
        self.keysyms_per_code = usize::from(r[1]);
        self.map = r[32..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        Ok(())
    }

    /// The keycode that types `sym` (lowercase letters map through their keysym).
    fn keycode(&self, sym: u32) -> Option<u8> {
        if self.keysyms_per_code == 0 {
            return None;
        }
        let index = self
            .map
            .chunks(self.keysyms_per_code)
            .position(|syms| syms.contains(&sym))?;
        self.min_keycode.checked_add(u8::try_from(index).ok()?)
    }

    fn fake(&mut self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        let mut req = vec![self.xtest, 2, 9, 0, kind, detail, 0, 0];
        req.extend_from_slice(&0u32.to_le_bytes()); // time: now
        req.extend_from_slice(&self.root.to_le_bytes());
        req.extend_from_slice(&[0; 8]);
        req.extend_from_slice(&x.to_le_bytes());
        req.extend_from_slice(&y.to_le_bytes());
        req.extend_from_slice(&[0; 8]); // pad, pad, device id 0 (core)
        self.request(&req)
    }

    /// A round trip (`GetInputFocus`), so every request before it reached the server.
    fn sync(&mut self) -> Result<(), String> {
        self.request(&[43, 0, 1, 0])?;
        self.reply().map(|_| ())
    }

    /// Gives the keyboard to the largest top-level window (the editor's; nothing else runs on
    /// the private display) and moves the pointer onto it. Without a window manager nobody else
    /// sets the input focus.
    ///
    /// # Errors
    ///
    /// No window, or a request failed.
    pub fn focus_editor(&mut self) -> Result<(), String> {
        let mut req = vec![15, 0, 2, 0];
        req.extend_from_slice(&self.root.to_le_bytes());
        self.request(&req)?;
        let tree = self.reply()?;
        let n = usize::from(u16_at(&tree, 16));
        let children: Vec<u32> = (0..n)
            .filter(|i| tree.len() >= 36 + i * 4)
            .map(|i| u32_at(&tree, 32 + i * 4))
            .collect();
        // (window, area, x, y, width, height) of the largest mapped child.
        let mut best: Option<Candidate> = None;
        for w in children {
            // GetWindowAttributes: only mapped windows (map-state byte 26 == 2, viewable).
            let mut req = vec![3, 0, 2, 0];
            req.extend_from_slice(&w.to_le_bytes());
            self.request(&req)?;
            let Ok(attrs) = self.reply() else { continue };
            if attrs.get(26) != Some(&2) {
                continue;
            }
            let mut req = vec![14, 0, 2, 0];
            req.extend_from_slice(&w.to_le_bytes());
            self.request(&req)?;
            let Ok(g) = self.reply() else { continue };
            let (x, y) = (u16_at(&g, 12).cast_signed(), u16_at(&g, 14).cast_signed());
            let (width, height) = (u16_at(&g, 16), u16_at(&g, 18));
            let area = u32::from(width) * u32::from(height);
            if best.is_none_or(|b| area > b.1) {
                best = Some((w, area, x, y, width, height));
            }
        }
        let (window, _, x, y, width, height) = best.ok_or("no mapped window on the display")?;
        // SetInputFocus(revert-to PointerRoot, the window, CurrentTime).
        let mut req = vec![42, 1, 3, 0];
        req.extend_from_slice(&window.to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes());
        self.request(&req)?;
        let cx = x.saturating_add(i16::try_from(width / 2).unwrap_or(0));
        let cy = y.saturating_add(i16::try_from(height / 2).unwrap_or(0));
        self.fake(6, 0, cx, cy)?;
        self.sync()
    }

    /// Presses `modifiers` (keysyms), taps `key`, releases the modifiers.
    ///
    /// # Errors
    ///
    /// A keysym has no keycode on this server, or a request failed.
    pub fn chord(&mut self, modifiers: &[u32], key: u32) -> Result<(), String> {
        let code = |x: &Self, s: u32| {
            x.keycode(s)
                .ok_or_else(|| format!("no keycode for keysym {s:#x}"))
        };
        let mods: Vec<u8> = modifiers
            .iter()
            .map(|m| code(self, *m))
            .collect::<Result<_, _>>()?;
        let k = code(self, key)?;
        for m in &mods {
            self.fake(2, *m, 0, 0)?;
        }
        self.fake(2, k, 0, 0)?;
        self.fake(3, k, 0, 0)?;
        for m in mods.iter().rev() {
            self.fake(3, *m, 0, 0)?;
        }
        self.sync()
    }

    /// Types lowercase ASCII letters and digits one by one.
    ///
    /// # Errors
    ///
    /// A character has no keycode, or a request failed.
    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        for c in text.chars() {
            self.chord(&[], u32::from(c))?;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_rounds_to_four_bytes() {
        assert_eq!(pad4(0), 0);
        assert_eq!(pad4(5), 3);
        assert_eq!(pad4(8), 0);
    }

    #[test]
    fn a_missing_display_is_an_error_not_a_panic() {
        assert!(XKeys::connect("not-a-display").is_err());
        assert!(XKeys::connect(":65000").is_err());
    }
}
