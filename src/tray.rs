//! Status-bar icon on Linux: StatusNotifierItem over D-Bus via `ksni`, no GTK. KDE shows it
//! natively; GNOME needs the "AppIndicator and KStatusNotifierItem Support" extension. Elsewhere
//! (and when no tray host is running) `spawn` returns `None` and closing the window quits.

use std::sync::mpsc;

// Only constructed by the Linux tray; elsewhere `spawn` is a stub.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum TrayCmd {
    Show,
    Music,
    Pomodoro,
    StopLive,
    Quit,
}

#[cfg(target_os = "linux")]
pub struct Tray {
    pub cmds: mpsc::Receiver<TrayCmd>,
    handle: ksni::blocking::Handle<Icon>,
}

#[cfg(target_os = "linux")]
impl Tray {
    /// Keeps the menu's "Stop live" in sync with the app.
    pub fn set_live(&self, live: bool) {
        self.handle.update(|t| t.live = live);
    }
}

#[cfg(target_os = "linux")]
struct Icon {
    tx: mpsc::Sender<TrayCmd>,
    wake: Box<dyn Fn() + Send>,
    live: bool,
}

#[cfg(target_os = "linux")]
impl Icon {
    fn send(&self, c: TrayCmd) {
        let _ = self.tx.send(c);
        (self.wake)();
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Icon {
    fn id(&self) -> String {
        "aula-control".into()
    }
    fn title(&self) -> String {
        "AULA F75 Max".into()
    }
    fn icon_name(&self) -> String {
        "input-keyboard".into() // freedesktop standard icon, present in every icon theme
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::Show);
    }
    /// The tray host went away (extension disabled, shell restarted): bring the window back so
    /// the app is never stranded hidden. Keep the service so the icon returns with the host.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.send(TrayCmd::Show);
        true
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        let item = |label: &str, cmd: fn() -> TrayCmd, enabled: bool| -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |t: &mut Self| t.send(cmd())),
                ..Default::default()
            }
            .into()
        };
        vec![
            item("Open window", || TrayCmd::Show, true),
            MenuItem::Separator,
            item("Live: Music", || TrayCmd::Music, !self.live),
            item("Live: Pomodoro", || TrayCmd::Pomodoro, !self.live),
            item("Stop live", || TrayCmd::StopLive, self.live),
            MenuItem::Separator,
            item("Quit", || TrayCmd::Quit, true),
        ]
    }
}

/// `wake` is called after every command so the (possibly hidden) window processes it.
#[cfg(target_os = "linux")]
pub fn spawn(wake: Box<dyn Fn() + Send>) -> Option<Tray> {
    use ksni::blocking::TrayMethods;
    let (tx, cmds) = mpsc::channel();
    match (Icon { tx, wake, live: false }).spawn() {
        Ok(handle) => Some(Tray { cmds, handle }),
        Err(e) => {
            eprintln!("aula-control: no status-bar icon ({e}); closing the window will quit");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub struct Tray {
    pub cmds: mpsc::Receiver<TrayCmd>,
}

#[cfg(not(target_os = "linux"))]
impl Tray {
    pub fn set_live(&self, _live: bool) {}
}

#[cfg(not(target_os = "linux"))]
pub fn spawn(_wake: Box<dyn Fn() + Send>) -> Option<Tray> {
    None
}
