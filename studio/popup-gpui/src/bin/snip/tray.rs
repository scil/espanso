//! Tray icon and its menu. See `studio/EDITOR.md`, step 2.
//!
//! The icon lives on the gpui main thread, which runs the Windows message
//! loop. Menu clicks arrive on the global `MenuEvent` channel, and the popup
//! reads them in its tick with [`Tray::poll`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// What the user picked in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewSnippet,
    EditSnippets,
    Pause(bool),
    Reload,
    OpenFolder,
    OpenLog,
    StartAtSignIn(bool),
    RestartAsAdmin,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    pause: CheckMenuItem,
    sign_in: CheckMenuItem,
    paused: bool,
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "snip";

impl Tray {
    /// Creates the icon. `admin` is true when this process already has
    /// administrator rights, so the menu hides "Restart as administrator".
    pub fn new(admin: bool) -> Result<Tray, String> {
        let pause = CheckMenuItem::with_id("pause", "Pause expansion", true, false, None);
        let sign_in =
            CheckMenuItem::with_id("sign-in", "Start at sign-in", true, start_at_sign_in(), None);
        let menu = Menu::new();
        let sep = PredefinedMenuItem::separator();
        menu.append(&MenuItem::with_id("new", "New snippet", true, None)).map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("edit", "Edit snippets", true, None))
            .map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
        menu.append(&pause).map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("reload", "Reload snippets", true, None))
            .map_err(|e| e.to_string())?;
        menu.append(&sep).map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("folder", "Open configuration folder", true, None))
            .map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("log", "Open log", true, None))
            .map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
        menu.append(&sign_in).map_err(|e| e.to_string())?;
        if !admin && admin_script().is_some() {
            menu.append(&MenuItem::with_id("admin", "Restart as administrator", true, None))
                .map_err(|e| e.to_string())?;
        }
        menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("quit", "Quit", true, None)).map_err(|e| e.to_string())?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("snip: loading snippets")
            .with_icon(draw_icon(false))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Tray { icon, pause, sign_in, paused: false })
    }

    /// The menu clicks and icon double clicks since the last call.
    pub fn poll(&self) -> Vec<Action> {
        let double = TrayIconEvent::receiver()
            .try_iter()
            .filter(|ev| matches!(ev, TrayIconEvent::DoubleClick { .. }))
            .map(|_| Action::EditSnippets);
        let menu = MenuEvent::receiver()
            .try_iter()
            .filter_map(|ev| match ev.id.as_ref() {
                "new" => Some(Action::NewSnippet),
                "edit" => Some(Action::EditSnippets),
                "pause" => Some(Action::Pause(self.pause.is_checked())),
                "reload" => Some(Action::Reload),
                "folder" => Some(Action::OpenFolder),
                "log" => Some(Action::OpenLog),
                "sign-in" => Some(Action::StartAtSignIn(self.sign_in.is_checked())),
                "admin" => Some(Action::RestartAsAdmin),
                "quit" => Some(Action::Quit),
                _ => None,
            });
        double.chain(menu).collect()
    }

    /// Shows the state in the icon and the tooltip.
    pub fn set_status(&mut self, paused: bool, text: &str) {
        if paused != self.paused {
            self.paused = paused;
            let _ = self.icon.set_icon(Some(draw_icon(paused)));
        }
        // Windows cuts a tooltip at 127 characters.
        let tip: String = format!("snip: {text}").chars().take(120).collect();
        let _ = self.icon.set_tooltip(Some(tip));
    }

    /// Puts the check mark back after a failed change.
    pub fn set_sign_in_checked(&self, on: bool) {
        self.sign_in.set_checked(on);
    }
}

/// A 32 by 32 icon: a rounded square with three lines of "text". Gray
/// while paused.
fn draw_icon(paused: bool) -> Icon {
    const N: u32 = 32;
    let bg = if paused { [0x80, 0x80, 0x80] } else { [0x2f, 0x6f, 0xeb] };
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    let radius = 7.0f32;
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance outside the rounded corner, for a soft edge.
            let cx = fx.clamp(radius, N as f32 - radius);
            let cy = fy.clamp(radius, N as f32 - radius);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - radius;
            let alpha = (0.5 - d).clamp(0.0, 1.0);
            // Three text lines of different length.
            let line = [(9, 23), (15, 23), (21, 17)]
                .iter()
                .any(|&(top, right)| y >= top && y < top + 3 && x >= 7 && x < right);
            let rgb = if line { [0xff, 0xff, 0xff] } else { bg };
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (alpha * 255.0) as u8]);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("valid icon size")
}

/// Runs a program without a console window.
fn quiet(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Whether the `Run` key starts snip at sign-in.
pub fn start_at_sign_in() -> bool {
    quiet("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Adds or removes the `Run` value. The value repeats the command line of
/// this process, so snip starts with the same options.
pub fn set_start_at_sign_in(on: bool) -> Result<(), String> {
    let output = if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut line = quote(exe.as_os_str());
        for arg in std::env::args_os().skip(1) {
            line.push(' ');
            line.push_str(&quote(&arg));
        }
        quiet("reg").args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/f", "/d"]).arg(line).output()
    } else {
        quiet("reg").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]).output()
    };
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn quote(arg: &std::ffi::OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() || s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

/// `scripts/win/snip.ps1` next to the build folder, if it is there. The
/// program runs from `studio/target/release`.
fn admin_script() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let studio = exe.parent()?.parent()?.parent()?;
    let script = studio.join("scripts").join("win").join("snip.ps1");
    script.is_file().then_some(script)
}

/// Starts the script that stops this process and starts snip again with
/// administrator rights. Windows asks for consent (UAC).
pub fn restart_as_admin() -> Result<(), String> {
    let script = admin_script().ok_or("scripts/win/snip.ps1 not found")?;
    let args: [OsString; 5] = [
        "-NoProfile".into(),
        "-File".into(),
        script.into_os_string(),
        "-Action".into(),
        "start".into(),
    ];
    quiet("pwsh").args(args).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Opens a folder in Explorer, or a file in its default program. The call
/// runs on a thread of its own, so the UI thread never waits for the shell.
/// An error goes to the log.
pub fn open(path: &Path) -> Result<(), String> {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if let Err(e) = popup_core::focus::shell_open(&path) {
            popup_core::log::line(&format!("tray: cannot open {}: {e}", path.display()));
        }
    });
    Ok(())
}
