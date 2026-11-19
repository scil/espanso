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
    Settings,
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
/// Explorer keeps the on/off state of each `Run` value here (the switch in
/// Task Manager and in Settings > Apps > Startup). On recent Windows 11
/// builds, Explorer skips a `Run` value that has no entry here.
const APPROVED_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// 12 bytes: the first byte 02 means enabled, the other bytes hold the time
/// of the last change and are zero when nobody switched it off.
const APPROVED_ON: &str = "020000000000000000000000";

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
        menu.append(&MenuItem::with_id("settings", "Settings...", true, None))
            .map_err(|e| e.to_string())?;
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
                "settings" => Some(Action::Settings),
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

    /// Puts the check mark back after a failed change, or shows a change
    /// made in the settings window.
    pub fn set_sign_in_checked(&self, on: bool) {
        self.sign_in.set_checked(on);
    }

    /// Shows a pause made in the settings window.
    pub fn set_pause_checked(&self, on: bool) {
        self.pause.set_checked(on);
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

/// Whether Explorer starts snip at sign-in: the `Run` value exists and its
/// entry under `StartupApproved\Run` says enabled.
pub fn start_at_sign_in() -> bool {
    let run = quiet("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .output()
        .is_ok_and(|o| o.status.success());
    run && quiet("reg")
        .args(["query", APPROVED_KEY, "/v", RUN_VALUE])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| approved_enabled(&String::from_utf8_lossy(&o.stdout)))
}

/// Reads the output of `reg query` for one `REG_BINARY` value. An even
/// first byte (02, 06) means enabled, an odd one (03, 07) means disabled.
fn approved_enabled(reg_output: &str) -> bool {
    reg_output
        .lines()
        .find_map(|l| l.split_once("REG_BINARY"))
        .and_then(|(_, data)| u8::from_str_radix(data.trim().get(..2)?, 16).ok())
        .is_some_and(|b| b & 1 == 0)
}

/// Adds or removes the `Run` value and its `StartupApproved` entry. The
/// value repeats the command line of this process, so snip starts with the
/// same options.
pub fn set_start_at_sign_in(on: bool) -> Result<(), String> {
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut line = quote(exe.as_os_str());
        for arg in std::env::args_os().skip(1) {
            line.push(' ');
            line.push_str(&quote(&arg));
        }
        reg(quiet("reg").args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/f", "/d"]).arg(line))?;
        reg(quiet("reg").args([
            "add",
            APPROVED_KEY,
            "/v",
            RUN_VALUE,
            "/t",
            "REG_BINARY",
            "/f",
            "/d",
            APPROVED_ON,
        ]))
    } else {
        reg(quiet("reg").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]))?;
        // The entry may be missing. Explorer ignores an entry without its
        // `Run` value, so a failure here does no harm.
        let _ = quiet("reg").args(["delete", APPROVED_KEY, "/v", RUN_VALUE, "/f"]).output();
        Ok(())
    }
}

fn reg(cmd: &mut Command) -> Result<(), String> {
    match cmd.output() {
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

/// Whether "Restart as administrator" can work: its script is there.
pub fn can_restart_as_admin() -> bool {
    admin_script().is_some()
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

/// Starts a new snip with the command line of this process. The caller
/// quits right after. The new process has the rights of this one, so an
/// administrator snip restarts without a consent prompt.
pub fn restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(exe).args(std::env::args_os().skip(1)).spawn().map(|_| ()).map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::approved_enabled;

    fn query(data: &str) -> String {
        format!("\r\nHKEY_CURRENT_USER\\StartupApproved\\Run\r\n    snip    REG_BINARY    {data}\r\n\r\n")
    }

    #[test]
    fn reads_the_first_byte() {
        assert!(approved_enabled(&query("020000000000000000000000")));
        assert!(approved_enabled(&query("060000000000000000000000")));
        assert!(!approved_enabled(&query("0300000067D879B22B95DC01")));
        assert!(!approved_enabled("ERROR: not found"));
    }
}
