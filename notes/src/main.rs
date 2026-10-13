//! Acme Notes: a small notes app with a window, used to test ToDesktop v2
//! smoke tests. It keeps notes in $XDG_DATA_HOME/acme-notes/notes.json and
//! updates itself through the ToDesktop updater (tarball and AppImage
//! installs; apt, dnf and pacman update package installs).

use std::path::PathBuf;

use eframe::egui;
use serde::{Deserialize, Serialize};
use todesktop_updater::{UpdateState, Updater, Updates};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Note {
    title: String,
    body: String,
    #[serde(default)]
    pinned: bool,
}

/// notes.json since 1.1.0: versioned, so later formats can migrate.
#[derive(Debug, Deserialize, Serialize)]
struct NotesFile {
    version: u32,
    notes: Vec<Note>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local")
                .join("share")
        })
        .join("acme-notes")
}

fn notes_file() -> PathBuf {
    data_dir().join("notes.json")
}

fn sample_notes() -> Vec<Note> {
    vec![
        Note {
            title: "Groceries".into(),
            body: "Oat milk\nCoffee beans\nLemons\nSourdough".into(),
            pinned: true,
        },
        Note {
            title: "Q3 planning".into(),
            body: "Ship search in the sidebar.\nMove sync to the new API.\nHire a second designer."
                .into(),
            pinned: false,
        },
        Note {
            title: "Reading list".into(),
            body: "The Design of Everyday Things\nA Philosophy of Software Design".into(),
            pinned: false,
        },
    ]
}

fn load_notes() -> Vec<Note> {
    match std::fs::read_to_string(notes_file()) {
        Ok(text) => match serde_json::from_str::<NotesFile>(&text) {
            Ok(file) => file.notes,
            // Before 1.1.0, notes.json was a plain list of notes.
            Err(_) => serde_json::from_str(&text).expect("notes.json is not valid"),
        },
        Err(_) => sample_notes(),
    }
}

fn save_notes(notes: &[Note]) {
    let path = notes_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let file = NotesFile {
        version: 2,
        notes: notes.to_vec(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&file) {
        std::fs::write(path, text).ok();
    }
}

struct NotesApp {
    notes: Vec<Note>,
    selected: usize,
    search: String,
    updates: Option<Updates>,
    version: String,
    frames: u64,
}

impl NotesApp {
    fn status(&self) -> String {
        let Some(updates) = &self.updates else {
            return "Not installed by ToDesktop".into();
        };
        let info = updates.info();
        let state = match updates.state() {
            UpdateState::Idle { .. } => "Up to date".to_owned(),
            UpdateState::Checking => "Checking for updates…".to_owned(),
            UpdateState::Available { update } => format!("Update available: {}", update.version),
            UpdateState::Downloading { update, progress } => {
                format!("Downloading {} · {:.0}%", update.version, progress * 100.0)
            }
            UpdateState::Ready { update } => format!("{} is ready; restarting", update.version),
            UpdateState::Error { message, .. } => format!("Update error: {message}"),
            UpdateState::Managed { by } => format!("Updated by {by}"),
        };
        format!("Build {} · {state}", info.build_number)
    }
}

impl eframe::App for NotesApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if let Some(updates) = &self.updates {
            if self.frames == 1 {
                updates.updater().report("window-shown", serde_json::json!({}));
            }
            // The window has painted a few frames: this launch works.
            if self.frames == 3 {
                updates.health_ready();
            }
        }
        let mut changed = false;

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(format!("Acme Notes {}", self.version));
                ui.separator();
                ui.label(self.status());
            });
            ui.add_space(4.0);
        });

        egui::SidePanel::left("notes")
            .exact_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Notes");
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search notes")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                let query = self.search.to_lowercase();
                let mut order: Vec<usize> = (0..self.notes.len())
                    .filter(|&index| {
                        let note = &self.notes[index];
                        query.is_empty()
                            || note.title.to_lowercase().contains(&query)
                            || note.body.to_lowercase().contains(&query)
                    })
                    .collect();
                // Pinned notes first.
                order.sort_by_key(|&index| !self.notes[index].pinned);
                for index in order {
                    let note = &self.notes[index];
                    let title = match (note.title.is_empty(), note.pinned) {
                        (true, _) => "Untitled".to_owned(),
                        (false, true) => format!("★ {}", note.title),
                        (false, false) => note.title.clone(),
                    };
                    if ui
                        .selectable_label(index == self.selected, title)
                        .clicked()
                    {
                        self.selected = index;
                    }
                }
                ui.add_space(8.0);
                if ui.button("New note").clicked() {
                    self.notes.push(Note {
                        title: String::new(),
                        body: String::new(),
                        pinned: false,
                    });
                    self.selected = self.notes.len() - 1;
                    changed = true;
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(note) = self.notes.get_mut(self.selected) {
                ui.add_space(8.0);
                changed |= ui.checkbox(&mut note.pinned, "Pinned").changed();
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut note.title)
                            .hint_text("Title")
                            .font(egui::TextStyle::Heading)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();
                ui.add_space(8.0);
                changed |= ui
                    .add(
                        egui::TextEdit::multiline(&mut note.body)
                            .desired_width(f32::INFINITY)
                            .desired_rows(18),
                    )
                    .changed();
            }
        });

        if changed {
            save_notes(&self.notes);
        }
        // Keep the status line current while an update runs.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn main() -> eframe::Result {
    if std::env::args().any(|arg| arg == "--version") {
        let version = Updater::from_current_exe()
            .map(|updater| updater.identity.version)
            .unwrap_or_else(|_| "dev".into());
        println!("acme-notes {version}");
        return Ok(());
    }
    let updates = Updater::from_current_exe().ok().map(Updates::new);
    let version = updates
        .as_ref()
        .map(|updates| updates.info().version)
        .unwrap_or_else(|| "dev".into());
    if let Some(updates) = &updates {
        // A desktop app: nothing restarts it, so the updater starts the
        // new version itself.
        updates.relaunch_by_spawning();
        updates.start();
    }

    let notes = load_notes();
    if !notes_file().exists() {
        save_notes(&notes);
    }
    let app = NotesApp {
        notes,
        selected: 0,
        search: String::new(),
        updates,
        version,
        frames: 0,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Acme Notes")
            .with_inner_size([960.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Acme Notes",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            cc.egui_ctx.style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(8.0, 6.0);
                style.spacing.button_padding = egui::vec2(10.0, 5.0);
            });
            Ok(Box::new(app))
        }),
    )
}
