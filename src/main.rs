use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arboard::Clipboard;
use eframe::egui::{self, Color32, FontId, Key, RichText, Rounding, Stroke};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::traits::Stream;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
use rand::Rng;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Prompt {
    id: String,
    title: String,
    text: String,
    fav: bool,
    uses: u64,
    created: u64,
    updated: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Idea {
    id: i64,
    body: String,
    project: String,
    status: String,
    created_at: String,
}

fn db_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local/share")
    });
    base.join("pvault").join("pvault.db")
}

fn init_db() -> Result<Connection, rusqlite::Error> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let conn = Connection::open(&path)?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;

         CREATE TABLE IF NOT EXISTS prompts (
             id TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             text TEXT NOT NULL,
             fav INTEGER NOT NULL DEFAULT 0,
             uses INTEGER NOT NULL DEFAULT 0,
             created INTEGER NOT NULL,
             updated INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS ideas (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             body TEXT NOT NULL,
             project TEXT NOT NULL DEFAULT '',
             status TEXT NOT NULL DEFAULT 'inbox',
             created_at TEXT NOT NULL
         );",
    )?;

    // Auto-migrate from JSON if prompts table is empty
    let count: i64 = conn.query_row("SELECT count(*) FROM prompts", [], |r| r.get(0))?;
    if count == 0 {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let json_paths = [
            PathBuf::from(&home).join(".config/pvault/prompts.json"),
            PathBuf::from(&home).join(".config/prompt-vault/prompts.json"),
        ];
        for jp in &json_paths {
            if let Ok(data) = fs::read_to_string(jp) {
                if let Ok(json_prompts) = serde_json::from_str::<Vec<serde_json::Value>>(&data) {
                    for p in json_prompts {
                        let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let fav = p.get("fav").and_then(|v| v.as_bool()).unwrap_or(false);
                        let uses = p.get("uses").and_then(|v| v.as_u64()).unwrap_or(0);
                        let created = p.get("created").and_then(|v| v.as_u64()).unwrap_or(0);
                        let updated = p.get("updated").and_then(|v| v.as_u64()).unwrap_or(0);
                        if !id.is_empty() && !title.is_empty() {
                            let _ = conn.execute(
                                "INSERT OR IGNORE INTO prompts (id, title, text, fav, uses, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                                params![id, title, text, if fav { 1 } else { 0 }, uses, created, updated],
                            );
                        }
                    }
                    break;
                }
            }
        }
    }

    // Auto-migrate from existing jot.db if ideas table is empty
    let ideas_count: i64 = conn.query_row("SELECT count(*) FROM ideas", [], |r| r.get(0))?;
    if ideas_count == 0 {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let old_jot_db = PathBuf::from(&home).join(".local/share/jot/jot.db");
        if old_jot_db.exists() {
            if let Ok(old_conn) = Connection::open(&old_jot_db) {
                if let Ok(mut stmt) = old_conn.prepare("SELECT body, COALESCE(project, ''), status, created_at FROM ideas WHERE status = 'inbox'") {
                    if let Ok(rows) = stmt.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    }) {
                        for row in rows.flatten() {
                            let _ = conn.execute(
                                "INSERT INTO ideas (body, project, status, created_at) VALUES (?1, ?2, ?3, ?4)",
                                params![row.0, row.1, row.2, row.3],
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(conn)
}

fn load_prompts(conn: &Connection) -> Vec<Prompt> {
    let mut stmt = match conn.prepare("SELECT id, title, text, fav, uses, created, updated FROM prompts ORDER BY fav DESC, uses DESC, updated DESC") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(Prompt {
            id: r.get(0)?,
            title: r.get(1)?,
            text: r.get(2)?,
            fav: r.get::<_, i64>(3)? != 0,
            uses: r.get::<_, i64>(4)? as u64,
            created: r.get::<_, i64>(5)? as u64,
            updated: r.get::<_, i64>(6)? as u64,
        })
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

fn load_ideas(conn: &Connection, project_filter: Option<&str>) -> Vec<Idea> {
    let sql = if let Some(proj) = project_filter {
        format!("SELECT id, body, project, status, created_at FROM ideas WHERE status = 'inbox' AND project = '{}' ORDER BY id DESC", proj)
    } else {
        "SELECT id, body, project, status, created_at FROM ideas WHERE status = 'inbox' ORDER BY id DESC".to_string()
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(Idea {
            id: r.get(0)?,
            body: r.get(1)?,
            project: r.get(2)?,
            status: r.get(3)?,
            created_at: r.get(4)?,
        })
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

fn load_distinct_projects(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT DISTINCT project FROM ideas WHERE status = 'inbox' AND project != '' ORDER BY project ASC") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| r.get::<_, String>(0));
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_id() -> String {
    let mut rng = rand::thread_rng();
    let num: u128 = rng.gen();
    format!("{:016x}", num)
}

const SOCKET_NAME: &str = "pvault-launcher.sock";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let initial_mode = if args.iter().any(|arg| arg == "--jot" || arg == "--queue") {
        TabMode::Queue
    } else {
        TabMode::Vault
    };

    let socket_name = SOCKET_NAME.to_ns_name::<GenericNamespaced>().unwrap();
    if let Ok(mut stream) = interprocess::local_socket::Stream::connect(socket_name.clone()) {
        let msg = if initial_mode == TabMode::Queue { "TOGGLE_QUEUE\n" } else { "TOGGLE_VAULT\n" };
        let _ = stream.write_all(msg.as_bytes());
        let _ = stream.flush();
        return Ok(());
    }

    let toggle_flag = Arc::new(AtomicBool::new(false));
    let thread_flag = toggle_flag.clone();

    let listener = ListenerOptions::new().name(socket_name).create_sync();
    if let Ok(listener) = listener {
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut reader = BufReader::new(conn);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                thread_flag.store(true, Ordering::SeqCst);
            }
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([660.0, 440.0])
            .with_min_inner_size([520.0, 320.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "Prompt Vault",
        options,
        Box::new(move |cc| {
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals.dark_mode = true;
            style.visuals.panel_fill = Color32::TRANSPARENT;
            style.visuals.window_fill = Color32::TRANSPARENT;
            cc.egui_ctx.set_style(style);
            Ok(Box::new(PvaultApp::new(initial_mode)))
        }),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum TabMode {
    Vault,
    Queue,
}

#[derive(PartialEq)]
enum AppSubMode {
    List,
    Create,
}

struct PvaultApp {
    conn: Connection,
    tab: TabMode,
    sub_mode: AppSubMode,
    prompts: Vec<Prompt>,
    ideas: Vec<Idea>,
    projects: Vec<String>,
    selected_project_idx: usize, // 0 = All
    query: String,
    selected_index: usize,
    new_title: String,
    new_text: String,
    new_project: String,
    matcher: SkimMatcherV2,
    request_focus: bool,
    scroll_to_selected: bool,
}

impl PvaultApp {
    fn new(initial_mode: TabMode) -> Self {
        let conn = init_db().expect("failed to open database");
        let prompts = load_prompts(&conn);
        let projects = load_distinct_projects(&conn);
        let ideas = load_ideas(&conn, None);

        Self {
            conn,
            tab: initial_mode,
            sub_mode: AppSubMode::List,
            prompts,
            ideas,
            projects,
            selected_project_idx: 0,
            query: String::new(),
            selected_index: 0,
            new_title: String::new(),
            new_text: String::new(),
            new_project: String::new(),
            matcher: SkimMatcherV2::default(),
            request_focus: true,
            scroll_to_selected: false,
        }
    }

    fn reload_data(&mut self) {
        self.prompts = load_prompts(&self.conn);
        self.projects = load_distinct_projects(&self.conn);
        let proj_filter = if self.selected_project_idx == 0 {
            None
        } else {
            self.projects.get(self.selected_project_idx - 1).map(|s| s.as_str())
        };
        self.ideas = load_ideas(&self.conn, proj_filter);
    }

    fn filtered_prompt_indices(&self) -> Vec<usize> {
        if self.query.trim().is_empty() {
            return (0..self.prompts.len()).collect();
        }
        let mut scored: Vec<(i64, usize)> = self
            .prompts
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| {
                let title_score = self.matcher.fuzzy_match(&p.title, &self.query).unwrap_or(0);
                let text_score = self.matcher.fuzzy_match(&p.text, &self.query).unwrap_or(0);
                let max_score = title_score.max(text_score);
                if max_score > 0 {
                    let bonus = if p.fav { 100 } else { 0 };
                    Some((max_score + bonus, idx))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, idx)| idx).collect()
    }

    fn filtered_idea_indices(&self) -> Vec<usize> {
        if self.query.trim().is_empty() {
            return (0..self.ideas.len()).collect();
        }
        let mut scored: Vec<(i64, usize)> = self
            .ideas
            .iter()
            .enumerate()
            .filter_map(|(idx, idea)| {
                let body_score = self.matcher.fuzzy_match(&idea.body, &self.query).unwrap_or(0);
                let proj_score = self.matcher.fuzzy_match(&idea.project, &self.query).unwrap_or(0);
                let max_score = body_score.max(proj_score);
                if max_score > 0 {
                    Some((max_score, idx))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, idx)| idx).collect()
    }

    fn copy_text(&self, text: &str, simulate_paste: bool) {
        if let Ok(mut cb) = Clipboard::new() {
            let _ = cb.set_text(text.to_string());
        }
        // Copy to standard Wayland clipboard
        let _ = std::process::Command::new("wl-copy").arg(text).spawn();
        // Also copy to Linux Primary Selection (middle-click buffer)
        let _ = std::process::Command::new("wl-copy")
            .arg("--primary")
            .arg(text)
            .spawn();

        if simulate_paste {
            let t = text.to_string();
            std::thread::spawn(move || {
                // Wait for the window to close and KWin to refocus the previous active window
                std::thread::sleep(std::time::Duration::from_millis(250));

                // Try wtype first (native Wayland modifier paste Ctrl+Shift+V)
                let wtype_res = std::process::Command::new("wtype")
                    .args(["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"])
                    .status();

                // If wtype fails, try typing text directly or fallback
                if wtype_res.is_err() {
                    let _ = std::process::Command::new("xdotool")
                        .args(["key", "--clearmodifiers", "ctrl+shift+v"])
                        .spawn();
                }
            });
        }
    }

    fn consume_prompt(&mut self, orig_idx: usize, simulate_paste: bool, ctx: &egui::Context) {
        let p = &self.prompts[orig_idx];
        let text = p.text.clone();
        let id = p.id.clone();
        let new_uses = p.uses + 1;
        let now = now_millis();
        let _ = self.conn.execute(
            "UPDATE prompts SET uses = ?1, updated = ?2 WHERE id = ?3",
            params![new_uses, now, id],
        );
        self.copy_text(&text, simulate_paste);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn consume_idea(&mut self, orig_idx: usize, mark_done: bool, simulate_paste: bool, ctx: &egui::Context) {
        let idea = &self.ideas[orig_idx];
        let text = idea.body.clone();
        let id = idea.id;
        if mark_done {
            let _ = self.conn.execute("UPDATE ideas SET status = 'done' WHERE id = ?1", params![id]);
        }
        self.copy_text(&text, simulate_paste);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for PvaultApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let prompt_indices = self.filtered_prompt_indices();
        let idea_indices = self.filtered_idea_indices();
        let total_items = match self.tab {
            TabMode::Vault => prompt_indices.len(),
            TabMode::Queue => idea_indices.len(),
        };

        let mut do_escape = false;
        let mut do_tab = false;
        let mut do_ctrl_n = false;
        let mut do_ctrl_p = false;
        let mut do_up = false;
        let mut do_down = false;
        let mut do_fav = false;
        let mut do_enter = false;
        let mut is_shift = false;

        ctx.input(|i| {
            if i.key_pressed(Key::Escape) {
                do_escape = true;
            }
            if i.key_pressed(Key::Tab) {
                do_tab = true;
            }
            if i.modifiers.ctrl && i.key_pressed(Key::P) {
                do_ctrl_p = true;
            }
            if i.modifiers.ctrl && i.key_pressed(Key::N) {
                do_ctrl_n = true;
            }

            if self.sub_mode == AppSubMode::List {
                if i.key_pressed(Key::ArrowDown) || (i.modifiers.ctrl && i.key_pressed(Key::J)) {
                    do_down = true;
                }
                if i.key_pressed(Key::ArrowUp) || (i.modifiers.ctrl && i.key_pressed(Key::K)) {
                    do_up = true;
                }
                if i.modifiers.ctrl && i.key_pressed(Key::F) {
                    do_fav = true;
                }
                if i.key_pressed(Key::Enter) {
                    do_enter = true;
                    is_shift = i.modifiers.shift;
                }
            }
        });

        if do_escape {
            if self.sub_mode == AppSubMode::Create {
                self.sub_mode = AppSubMode::List;
                self.request_focus = true;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        if do_tab {
            self.tab = match self.tab {
                TabMode::Vault => TabMode::Queue,
                TabMode::Queue => TabMode::Vault,
            };
            self.selected_index = 0;
            self.scroll_to_selected = true;
            self.request_focus = true;
        }

        if do_ctrl_p && self.tab == TabMode::Queue {
            let num_choices = self.projects.len() + 1; // 0 = All
            self.selected_project_idx = (self.selected_project_idx + 1) % num_choices;
            self.reload_data();
            self.selected_index = 0;
            self.scroll_to_selected = true;
        }

        if do_ctrl_n {
            self.sub_mode = AppSubMode::Create;
            self.new_title.clear();
            self.new_text.clear();
            self.new_project = if self.selected_project_idx > 0 {
                self.projects.get(self.selected_project_idx - 1).cloned().unwrap_or_default()
            } else {
                String::new()
            };
        }

        if do_down && total_items > 0 {
            self.selected_index = (self.selected_index + 1) % total_items;
            self.scroll_to_selected = true;
        }

        if do_up && total_items > 0 {
            self.selected_index = if self.selected_index == 0 {
                total_items - 1
            } else {
                self.selected_index - 1
            };
            self.scroll_to_selected = true;
        }

        if do_fav && self.tab == TabMode::Vault {
            if let Some(&orig_idx) = prompt_indices.get(self.selected_index) {
                let p = &self.prompts[orig_idx];
                let new_fav = if p.fav { 0 } else { 1 };
                let now = now_millis();
                let _ = self.conn.execute(
                    "UPDATE prompts SET fav = ?1, updated = ?2 WHERE id = ?3",
                    params![new_fav, now, p.id],
                );
                self.reload_data();
            }
        }

        if do_enter && total_items > 0 {
            match self.tab {
                TabMode::Vault => {
                    if let Some(&orig_idx) = prompt_indices.get(self.selected_index) {
                        self.consume_prompt(orig_idx, is_shift, ctx);
                        return;
                    }
                }
                TabMode::Queue => {
                    if let Some(&orig_idx) = idea_indices.get(self.selected_index) {
                        // Enter = mark done & consume; Shift+Enter = copy only (keep)
                        let mark_done = !is_shift;
                        self.consume_idea(orig_idx, mark_done, is_shift, ctx);
                        return;
                    }
                }
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::TRANSPARENT).inner_margin(egui::Margin::same(8.0)))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_rgb(17, 18, 25))
                    .rounding(Rounding::same(10.0))
                    .stroke(Stroke::new(1.5, Color32::from_rgb(120, 125, 170)))
                    .inner_margin(egui::Margin::same(0.0))
                    .show(ui, |ui| {
                        // Tab switcher bar
                        egui::Frame::none()
                            .fill(Color32::from_rgb(20, 22, 31))
                            .rounding(Rounding {
                                nw: 10.0,
                                ne: 10.0,
                                sw: 0.0,
                                se: 0.0,
                            })
                            .inner_margin(egui::Margin::symmetric(14.0, 7.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let vault_active = self.tab == TabMode::Vault;
                                    let v_color = if vault_active { Color32::from_rgb(167, 139, 250) } else { Color32::from_rgb(107, 114, 128) };
                                    if ui.add(egui::Label::new(RichText::new(format!("⚡ Prompts ({})", self.prompts.len())).color(v_color).font(FontId::proportional(13.0)).strong()).sense(egui::Sense::click())).clicked() {
                                        self.tab = TabMode::Vault;
                                        self.selected_index = 0;
                                    }

                                    ui.add_space(16.0);

                                    let queue_active = self.tab == TabMode::Queue;
                                    let q_color = if queue_active { Color32::from_rgb(56, 189, 248) } else { Color32::from_rgb(107, 114, 128) };
                                    if ui.add(egui::Label::new(RichText::new(format!("📥 Queue / Jot ({})", self.ideas.len())).color(q_color).font(FontId::proportional(13.0)).strong()).sense(egui::Sense::click())).clicked() {
                                        self.tab = TabMode::Queue;
                                        self.selected_index = 0;
                                    }

                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(RichText::new("Tab switch").color(Color32::from_rgb(107, 114, 128)).font(FontId::monospace(10.0)));
                                    });
                                });
                            });

                        ui.separator();

                        if self.sub_mode == AppSubMode::Create {
                            self.render_create_view(ui, ctx);
                        } else {
                            self.render_list_view(ui, ctx, &prompt_indices, &idea_indices);
                        }
                    });
            });
    }
}

impl PvaultApp {
    fn render_list_view(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        prompt_indices: &[usize],
        idea_indices: &[usize],
    ) {
        // Search bar & Project Tag
        egui::Frame::none()
            .fill(Color32::from_rgb(22, 23, 34))
            .inner_margin(egui::Margin::symmetric(14.0, 8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.tab == TabMode::Queue {
                        let proj_label = if self.selected_project_idx == 0 {
                            "📁 all".to_string()
                        } else {
                            format!("📁 {}", self.projects.get(self.selected_project_idx - 1).unwrap_or(&"".to_string()))
                        };
                        if ui.add(egui::Label::new(RichText::new(proj_label).color(Color32::from_rgb(56, 189, 248)).font(FontId::monospace(11.0)).strong()).sense(egui::Sense::click())).clicked() {
                            let num_choices = self.projects.len() + 1;
                            self.selected_project_idx = (self.selected_project_idx + 1) % num_choices;
                            self.reload_data();
                            self.selected_index = 0;
                        }
                        ui.add_space(4.0);
                    } else {
                        ui.label(RichText::new("⚡").color(Color32::from_rgb(167, 139, 250)).font(FontId::proportional(15.0)));
                    }

                    let hint = match self.tab {
                        TabMode::Vault => "Search prompts... (Ctrl+N new)",
                        TabMode::Queue => "Search ideas/tasks... (Ctrl+N jot)",
                    };

                    let response = ui.add_sized(
                        [ui.available_width() - 85.0, 24.0],
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(hint)
                            .font(FontId::proportional(13.5))
                            .text_color(Color32::from_rgb(243, 244, 246))
                            .frame(false),
                    );

                    if self.request_focus {
                        response.request_focus();
                        self.request_focus = false;
                    }

                    if response.changed() {
                        self.selected_index = 0;
                        self.scroll_to_selected = true;
                    }

                    ui.label(RichText::new("Esc close").color(Color32::from_rgb(107, 114, 128)).font(FontId::monospace(10.0)));
                });
            });

        ui.separator();

        let available_height = ui.available_height() - 32.0;
        let mut clicked_prompt_idx = None;
        let mut clicked_idea_idx = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(available_height)
            .show(ui, |ui| {
                ui.add_space(4.0);
                match self.tab {
                    TabMode::Vault => {
                        for (i, &orig_idx) in prompt_indices.iter().enumerate() {
                            let p = &self.prompts[orig_idx];
                            let is_selected = i == self.selected_index;
                            let bg_color = if is_selected {
                                Color32::from_rgb(38, 41, 58)
                            } else {
                                Color32::TRANSPARENT
                            };

                            let star_text = if p.fav { "★" } else { "☆" };
                            let star_color = if p.fav {
                                Color32::from_rgb(251, 191, 36)
                            } else {
                                Color32::from_rgb(75, 85, 99)
                            };
                            let title = p.title.clone();
                            let clean_snippet: String = p.text
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(75)
                                .collect();
                            let uses_str = format!("{} uses", p.uses);

                            let frame_res = egui::Frame::none()
                                .fill(bg_color)
                                .rounding(Rounding::same(6.0))
                                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                .show(ui, |ui: &mut egui::Ui| {
                                    ui.horizontal(|ui: &mut egui::Ui| {
                                        ui.label(RichText::new(star_text).color(star_color).font(FontId::proportional(14.0)));
                                        ui.add_space(4.0);
                                        ui.vertical(|ui: &mut egui::Ui| {
                                            ui.horizontal(|ui: &mut egui::Ui| {
                                                ui.label(RichText::new(title).color(Color32::from_rgb(243, 244, 246)).font(FontId::proportional(13.0)).strong());
                                                if is_selected {
                                                    ui.label(RichText::new("↵ copy").color(Color32::from_rgb(167, 139, 250)).font(FontId::monospace(9.0)));
                                                }
                                            });
                                            ui.label(RichText::new(clean_snippet).color(Color32::from_rgb(156, 163, 175)).font(FontId::proportional(11.0)));
                                        });

                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui: &mut egui::Ui| {
                                            ui.label(RichText::new(uses_str).color(Color32::from_rgb(107, 114, 128)).font(FontId::monospace(10.0)));
                                        });
                                    });
                                });

                            if is_selected && self.scroll_to_selected {
                                frame_res.response.scroll_to_me(Some(egui::Align::Center));
                            }
                            if frame_res.response.clicked() {
                                clicked_prompt_idx = Some(orig_idx);
                            }
                        }
                    }
                    TabMode::Queue => {
                        for (i, &orig_idx) in idea_indices.iter().enumerate() {
                            let idea = &self.ideas[orig_idx];
                            let is_selected = i == self.selected_index;
                            let bg_color = if is_selected {
                                Color32::from_rgb(31, 37, 56)
                            } else {
                                Color32::TRANSPARENT
                            };

                            let clean_body: String = idea.body
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(80)
                                .collect();
                            let proj_tag = if idea.project.is_empty() { "inbox".to_string() } else { idea.project.clone() };

                            let frame_res = egui::Frame::none()
                                .fill(bg_color)
                                .rounding(Rounding::same(6.0))
                                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                .show(ui, |ui: &mut egui::Ui| {
                                    ui.horizontal(|ui: &mut egui::Ui| {
                                        ui.label(RichText::new("📌").font(FontId::proportional(12.0)));
                                        ui.add_space(4.0);
                                        ui.vertical(|ui: &mut egui::Ui| {
                                            ui.horizontal(|ui: &mut egui::Ui| {
                                                ui.label(RichText::new(&proj_tag).color(Color32::from_rgb(56, 189, 248)).font(FontId::monospace(10.0)).strong());
                                                if is_selected {
                                                    ui.label(RichText::new("↵ copy & done").color(Color32::from_rgb(248, 113, 113)).font(FontId::monospace(9.0)));
                                                }
                                            });
                                            ui.label(RichText::new(clean_body).color(Color32::from_rgb(243, 244, 246)).font(FontId::proportional(12.0)));
                                        });
                                    });
                                });

                            if is_selected && self.scroll_to_selected {
                                frame_res.response.scroll_to_me(Some(egui::Align::Center));
                            }
                            if frame_res.response.clicked() {
                                clicked_idea_idx = Some(orig_idx);
                            }
                        }
                    }
                }
            });

        self.scroll_to_selected = false;

        if let Some(orig_idx) = clicked_prompt_idx {
            self.consume_prompt(orig_idx, false, ctx);
        }
        if let Some(orig_idx) = clicked_idea_idx {
            self.consume_idea(orig_idx, true, false, ctx);
        }

        // Bottom status bar
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            egui::Frame::none()
                .fill(Color32::from_rgb(13, 14, 20))
                .rounding(Rounding {
                    nw: 0.0,
                    ne: 0.0,
                    sw: 10.0,
                    se: 10.0,
                })
                .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let hint = match self.tab {
                            TabMode::Vault => "↵ Copy  •  ⇧↵ Paste  •  Ctrl+F Fav  •  Ctrl+N New",
                            TabMode::Queue => "↵ Copy & Done  •  ⇧↵ Copy (Keep)  •  Ctrl+P Project  •  Ctrl+N Jot",
                        };
                        ui.label(RichText::new(hint).color(Color32::from_rgb(107, 114, 128)).font(FontId::monospace(10.0)));
                    });
                });
        });
    }

    fn render_create_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        let header = match self.tab {
            TabMode::Vault => "New Reusable Prompt",
            TabMode::Queue => "Jot One-Time Idea / Task",
        };
        let accent = match self.tab {
            TabMode::Vault => Color32::from_rgb(167, 139, 250),
            TabMode::Queue => Color32::from_rgb(56, 189, 248),
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new(header).color(accent).font(FontId::proportional(15.0)).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("Esc cancel  •  Ctrl+S save").color(Color32::from_rgb(107, 114, 128)).font(FontId::monospace(10.0)));
            });
        });
        ui.add_space(8.0);

        if self.tab == TabMode::Vault {
            ui.label(RichText::new("Title").color(Color32::from_rgb(156, 163, 175)).font(FontId::proportional(12.0)));
            ui.add_sized(
                [ui.available_width(), 26.0],
                egui::TextEdit::singleline(&mut self.new_title)
                    .hint_text("Prompt title...")
                    .font(FontId::proportional(13.0)),
            );
            ui.add_space(8.0);
        } else {
            ui.label(RichText::new("Project (Optional)").color(Color32::from_rgb(156, 163, 175)).font(FontId::proportional(12.0)));
            ui.add_sized(
                [ui.available_width(), 26.0],
                egui::TextEdit::singleline(&mut self.new_project)
                    .hint_text("e.g. pvault, godot-game, or empty for general inbox")
                    .font(FontId::monospace(12.0)),
            );
            ui.add_space(8.0);
        }

        ui.label(RichText::new("Content").color(Color32::from_rgb(156, 163, 175)).font(FontId::proportional(12.0)));
        ui.add_sized(
            [ui.available_width(), ui.available_height() - 50.0],
            egui::TextEdit::multiline(&mut self.new_text)
                .hint_text("Enter text here...")
                .font(FontId::monospace(12.0)),
        );

        let mut do_save = false;
        ctx.input(|i| {
            if i.modifiers.ctrl && i.key_pressed(Key::S) {
                do_save = true;
            }
        });

        ui.horizontal(|ui| {
            if ui.button(RichText::new("Save").color(accent)).clicked() || do_save {
                if self.tab == TabMode::Vault {
                    if !self.new_title.trim().is_empty() && !self.new_text.trim().is_empty() {
                        let id = random_id();
                        let now = now_millis();
                        let _ = self.conn.execute(
                            "INSERT INTO prompts (id, title, text, fav, uses, created, updated) VALUES (?1, ?2, ?3, 1, 0, ?4, ?4)",
                            params![id, self.new_title.trim(), self.new_text.trim(), now],
                        );
                        self.reload_data();
                        self.sub_mode = AppSubMode::List;
                        self.request_focus = true;
                    }
                } else if !self.new_text.trim().is_empty() {
                    let now = chrono::Utc::now().to_rfc3339();
                    let proj = self.new_project.trim();
                    let _ = self.conn.execute(
                        "INSERT INTO ideas (body, project, status, created_at) VALUES (?1, ?2, 'inbox', ?3)",
                        params![self.new_text.trim(), proj, now],
                    );
                    self.reload_data();
                    self.sub_mode = AppSubMode::List;
                    self.request_focus = true;
                }
            }
            if ui.button("Cancel").clicked() {
                self.sub_mode = AppSubMode::List;
                self.request_focus = true;
            }
        });
    }
}
