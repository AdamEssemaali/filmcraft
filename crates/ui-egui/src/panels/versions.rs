//! Settings ▸ Versions: the FilmCraft releases on GitHub, each downloaded on request and switched
//! to by quitting (Save / Don't Save / Cancel first, as for Quit) and starting it on the same
//! project. The host ([`crate::VersionHost`]) does the downloading and starting.
//!
//! Commands: `app.versions` (the state), `app.versions.refresh`, `app.versions.download` and
//! `app.versions.switch` (`{"version": "0.5.0"}`). Automation ids: `settings.versions.refresh`,
//! `settings.versions.<version>.download|switch`.

use egui::{Color32, RichText, Ui};
use serde_json::{Value, json};

use crate::FilmcraftApp;

const NO_HOST: &str = "Version switching isn't available in this build.";

pub fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let action = id.strip_prefix("app.versions")?;
    let version = params.get("version").and_then(Value::as_str).unwrap_or_default().to_string();
    let r = match action {
        ".switch" => switch(app, ctx, &version),
        "" | ".refresh" | ".download" => {
            let Some(host) = app.hooks.versions.as_mut() else { return Some(Err(NO_HOST.into())) };
            match action {
                ".refresh" => host.refresh(),
                ".download" if version.is_empty() => return Some(Err("app.versions.download needs a `version`".into())),
                ".download" => host.download(&version),
                _ => {}
            }
            Ok(serde_json::to_value(host.state()).unwrap_or_default())
        }
        _ => return None,
    };
    Some(r)
}

/// Quit, then start `version` on the open project. With unsaved changes, Quit asks first; Cancel
/// there calls [`cancel_switch`].
pub fn switch(app: &mut FilmcraftApp, ctx: &egui::Context, version: &str) -> Result<Value, String> {
    let project = app.session.path.clone();
    let host = app.hooks.versions.as_mut().ok_or(NO_HOST)?;
    host.switch_to(version, project.as_deref())?;
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    Ok(json!({"switching": version}))
}

pub fn cancel_switch(app: &mut FilmcraftApp) {
    if let Some(h) = app.hooks.versions.as_mut() {
        h.cancel_switch();
    }
}

/// The page body (a custom row of the Versions category).
pub fn page(app: &mut FilmcraftApp, ui: &mut Ui) {
    let t = app.tokens;
    let note = |ui: &mut Ui, text: &str| {
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(t.text_dim)).wrap());
    };
    let Some(host) = app.hooks.versions.as_mut() else {
        note(ui, tl!("Version switching isn't available in this build."));
        return;
    };
    let s = host.state();
    if !s.checked && !s.loading {
        host.refresh();
    }
    if s.loading || s.downloading.is_some() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
    let mut action: Option<(&str, String)> = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(tlf!("Current version: {version}", version = s.current)).size(12.5).strong());
        let r = ui.add_enabled(!s.loading, egui::Button::new(tl!("Refresh")));
        app.auto.add("settings.versions.refresh", r.rect, "Refresh");
        if r.clicked() {
            action = Some(("refresh", String::new()));
        }
    });
    if !s.error.is_empty() {
        ui.colored_label(Color32::from_rgb(0xe0, 0x60, 0x60), &s.error);
    }
    if s.loading {
        note(ui, tl!("Checking GitHub for versions…"));
    } else if s.checked && s.releases.is_empty() && s.error.is_empty() {
        note(ui, tl!("No versions found."));
    }
    ui.add_space(4.0);
    egui::Grid::new("settings-versions").num_columns(3).spacing(egui::vec2(18.0, 6.0)).striped(true).show(ui, |ui| {
        for r in &s.releases {
            let mut name = RichText::new(format!("v{}", r.version)).size(12.5);
            if r.version == s.current {
                name = name.strong();
            }
            ui.horizontal(|ui| {
                ui.label(name);
                if r.prerelease {
                    ui.label(RichText::new(tl!("Pre-release")).size(11.0).color(t.text_dim));
                }
            });
            ui.label(RichText::new(&r.date).size(12.0).color(t.text_dim));
            let downloading = s.downloading.as_ref().filter(|d| d.version == r.version);
            if r.version == s.current {
                ui.label(RichText::new(tl!("Running")).size(12.0).color(t.accent));
            } else if let Some(d) = downloading {
                let percent = d.done.saturating_mul(100).checked_div(d.total).unwrap_or(0).min(100);
                ui.label(RichText::new(tlf!("Downloading… {percent}%", percent)).size(12.0));
            } else if r.installed {
                let b = ui.button(tl!("Switch"));
                app.auto.add(&format!("settings.versions.{}.switch", r.version), b.rect, "Switch");
                if b.clicked() {
                    action = Some(("switch", r.version.clone()));
                }
            } else if r.available {
                let b = ui.add_enabled(s.downloading.is_none(), egui::Button::new(tl!("Download")));
                app.auto.add(&format!("settings.versions.{}.download", r.version), b.rect, "Download");
                if b.clicked() {
                    action = Some(("download", r.version.clone()));
                }
            } else {
                ui.label(RichText::new(tl!("Not available for this system")).size(12.0).color(t.text_dim));
            }
            ui.end_row();
        }
    });
    ui.add_space(8.0);
    note(
        ui,
        tl!(
            "Switching quits FilmCraft, asking to save your changes first, and opens your project in the chosen version. Older versions may not open projects saved by newer ones."
        ),
    );
    if !s.folder.is_empty() {
        note(ui, &tlf!("Downloaded versions are kept in {folder}.", folder = s.folder));
    }
    let Some((what, version)) = action else { return };
    let ctx = ui.ctx().clone();
    let r = match what {
        "switch" => switch(app, &ctx, &version).map(|_| ()),
        _ => {
            if let Some(h) = app.hooks.versions.as_mut() {
                if what == "refresh" { h.refresh() } else { h.download(&version) }
            }
            Ok(())
        }
    };
    if let Err(e) = r {
        app.ui.status = e;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VersionHost, VersionsState};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Fake(Arc<Mutex<Option<String>>>);

    impl VersionHost for Fake {
        fn state(&self) -> VersionsState {
            VersionsState { current: "0.6.0".into(), checked: true, ..Default::default() }
        }
        fn refresh(&mut self) {}
        fn download(&mut self, _: &str) {}
        fn switch_to(&mut self, version: &str, _: Option<&str>) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(version.into());
            Ok(())
        }
        fn cancel_switch(&mut self) {
            *self.0.lock().unwrap() = None;
        }
    }

    /// A switch is handed to the host, and a cancelled quit takes it back.
    #[test]
    fn switch_goes_to_the_host_and_a_cancelled_quit_takes_it_back() {
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        assert_eq!(route(&mut app, &ctx, "app.versions.switch", &json!({"version": "0.5.0"})), Some(Err(NO_HOST.into())));
        let pending = Arc::new(Mutex::new(None));
        app.hooks.versions = Some(Box::new(Fake(pending.clone())));
        assert_eq!(route(&mut app, &ctx, "app.versions", &json!({})).and_then(Result::ok).map(|v| v["current"].clone()), Some(json!("0.6.0")));
        assert!(route(&mut app, &ctx, "app.versions.download", &json!({})).is_some_and(|r| r.is_err()));
        route(&mut app, &ctx, "app.versions.switch", &json!({"version": "0.5.0"})).unwrap().unwrap();
        assert_eq!(pending.lock().unwrap().as_deref(), Some("0.5.0"));
        cancel_switch(&mut app);
        assert_eq!(*pending.lock().unwrap(), None);
    }
}
