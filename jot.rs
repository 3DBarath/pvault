use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::io::{self, Read};
use std::path::PathBuf;
use eframe::egui;

#[derive(Parser)]
#[command(name = "jot", version, about = "capture-first idea/change inbox")]
struct Cli {
    #[arg(long)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Add {
        text: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    Search {
        query: String,
        #[arg(long)]
        json: bool,
    },
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    Set {
        id: i64,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        snooze: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    Done {
        id: i64,
    },
    Drop {
        id: i64,
    },
    Snooze {
        id: i64,
        dur: String,
    },
    Emit {
        id: i64,
    },
    Copy {
        id: i64,
    },
    Promote {
        id: i64,
        #[arg(long)]
        to: Option<String>,
    },
    Export {
        #[arg(long)]
        json: bool,
    },
    Import {
        file: String,
    },
    Path,
    Panel,
    Doctor,
    Edit {
        id: i64,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct Idea {
    id: i64,
    body: String,
    kind: String,
    status: String,
    project: Option<String>,
    file_path: Option<String>,
    tags: Option<String>,
    priority: i64,
    agent_task_id: Option<String>,
    snooze_until: Option<String>,
    created_at: String,
    updated_at: String,
}

fn db_path(cli_db: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli_db {
        return p;
    }
    if let Ok(e) = std::env::var("JOT_DB") {
        return PathBuf::from(e);
    }
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("jot").join("jot.db")
}

fn open_db(path: &PathBuf) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < 1 {
        conn.execute_batch(r#"
        CREATE TABLE IF NOT EXISTS ideas (
          id INTEGER PRIMARY KEY,
          body TEXT NOT NULL,
          kind TEXT NOT NULL DEFAULT 'idea',
          status TEXT NOT NULL DEFAULT 'inbox',
          project TEXT,
          file_path TEXT,
          tags TEXT,
          priority INTEGER NOT NULL DEFAULT 0,
          agent_task_id TEXT,
          snooze_until TEXT,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS ideas_fts USING fts5(
          body, tags, project, file_path,
          content='ideas', content_rowid='id'
        );
        CREATE TRIGGER IF NOT EXISTS ideas_ai AFTER INSERT ON ideas BEGIN
          INSERT INTO ideas_fts(rowid,body,tags,project,file_path) VALUES (new.id,new.body,new.tags,new.project,new.file_path);
        END;
        CREATE TRIGGER IF NOT EXISTS ideas_ad AFTER DELETE ON ideas BEGIN
          INSERT INTO ideas_fts(ideas_fts,rowid,body,tags,project,file_path) VALUES('delete',old.id,old.body,old.tags,old.project,old.file_path);
        END;
        CREATE TRIGGER IF NOT EXISTS ideas_au AFTER UPDATE ON ideas BEGIN
          INSERT INTO ideas_fts(ideas_fts,rowid,body,tags,project,file_path) VALUES('delete',old.id,old.body,old.tags,old.project,old.file_path);
          INSERT INTO ideas_fts(rowid,body,tags,project,file_path) VALUES (new.id,new.body,new.tags,new.project,new.file_path);
        END;
        "#)?;
        conn.execute("PRAGMA user_version = 1", [])?;
    }
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn row_to_idea(row: &rusqlite::Row) -> rusqlite::Result<Idea> {
    Ok(Idea {
        id: row.get(0)?,
        body: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        project: row.get(4)?,
        file_path: row.get(5)?,
        tags: row.get(6)?,
        priority: row.get(7)?,
        agent_task_id: row.get(8)?,
        snooze_until: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

const SEL: &str = "SELECT id,body,kind,status,project,file_path,tags,priority,agent_task_id,snooze_until,created_at,updated_at FROM ideas";

fn compose(idea: &Idea) -> String {
    let mut out = String::new();
    out.push_str(&idea.body);
    out.push_str("\n\n---\nContext:\n");
    out.push_str(&format!("  kind:    {}\n", idea.kind));
    out.push_str(&format!("  status:  {}\n", idea.status));
    if let Some(p) = &idea.project {
        out.push_str(&format!("  project: {}\n", p));
    }
    if let Some(f) = &idea.file_path {
        out.push_str(&format!("  file:    {}\n", f));
    }
    if let Some(t) = &idea.tags {
        out.push_str(&format!("  tags:    {}\n", t));
    }
    out.push_str(&format!("  priority:{}\n", idea.priority));
    out.push_str("\nWhen you are free, implement this as a change. Do not start until asked.\n");
    out
}

fn parse_dur(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('h') {
        let n: i64 = n.parse()?;
        return Ok(chrono::Duration::hours(n));
    }
    if let Some(n) = s.strip_suffix('d') {
        let n: i64 = n.parse()?;
        return Ok(chrono::Duration::days(n));
    }
    if let Some(n) = s.strip_suffix('m') {
        let n: i64 = n.parse()?;
        return Ok(chrono::Duration::minutes(n));
    }
    anyhow::bail!("invalid duration")
}

fn get_idea(conn: &Connection, id: i64) -> Result<Idea> {
    conn.query_row(&format!("{} WHERE id=?1", SEL), params![id], row_to_idea)
        .context("not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compose_template() {
        let i = Idea {
            id: 1,
            body: "b".into(),
            kind: "idea".into(),
            status: "inbox".into(),
            project: Some("p".into()),
            file_path: None,
            tags: Some("t".into()),
            priority: 1,
            agent_task_id: None,
            snooze_until: None,
            created_at: "".into(),
            updated_at: "".into(),
        };
        let c = compose(&i);
        assert!(c.contains("b\n\n---"));
        assert!(c.contains("project: p"));
        assert!(!c.contains("None"));
    }
    #[test]
    fn status_transition_snooze() {
        let d = parse_dur("2h").unwrap();
        assert_eq!(d.num_hours(), 2);
    }
}

struct JotPanel {
    db_path: PathBuf,
    conn: Option<Connection>,
    projects: Vec<String>,
    selected: usize, // 0 = All
    last_selected: usize,
    prompts: Vec<Idea>,
    input: String,
    new_project: String,
    use_new: bool,
    msg: String,
}

impl JotPanel {
    fn new(db_path: PathBuf) -> Self {
        let mut s = Self {
            db_path,
            conn: None,
            projects: vec![],
            selected: 0,
            last_selected: 0,
            prompts: vec![],
            input: String::new(),
            new_project: String::new(),
            use_new: false,
            msg: String::new(),
        };
        if let Some(p) = s.db_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match Connection::open(&s.db_path) {
            Ok(c) => {
                let _ = c.busy_timeout(std::time::Duration::from_secs(2));
                let _ = c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;");
                let _ = migrate(&c);
                s.conn = Some(c);
            }
            Err(e) => s.msg = format!("db error: {}", e),
        }
        s.reload();
        s
    }

    fn reload(&mut self) {
        let Some(conn) = &self.conn else { return; };
        // projects
        let mut ps = vec!["All".to_string()];
        if let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT project FROM ideas WHERE project IS NOT NULL ORDER BY project",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                for r in rows.flatten() {
                    ps.push(r);
                }
            }
        }
        self.projects = ps;
        if self.selected >= self.projects.len() {
            self.selected = 0;
        }
        // prompts
        let mut q = format!("{} WHERE status='inbox'", SEL);
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        if self.selected > 0 {
            q.push_str(" AND project=?1");
            params_vec.push(Box::new(self.projects[self.selected].clone()));
        }
        q.push_str(" ORDER BY id DESC");
        let mut out = vec![];
        if let Ok(mut stmt) = conn.prepare(&q) {
            let params: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
            if let Ok(rows) = stmt.query_map(params.as_slice(), row_to_idea) {
                for r in rows.flatten() {
                    out.push(r);
                }
            }
        }
        self.prompts = out;
    }

    fn project_name(&self) -> Option<String> {
        if self.selected == 0 {
            None
        } else {
            Some(self.projects[self.selected].clone())
        }
    }

    fn add(&mut self) {
        let body = self.input.trim().to_string();
        if body.is_empty() {
            return;
        }
        let proj = if self.use_new {
            let p = self.new_project.trim().to_string();
            if p.is_empty() { None } else { Some(p) }
        } else {
            self.project_name()
        };
        if let Some(conn) = &self.conn {
            let nows = now();
            let _ = conn.execute(
                "INSERT INTO ideas (body,kind,status,project,file_path,tags,priority,created_at,updated_at) VALUES (?1,'idea','inbox',?2,NULL,NULL,0,?3,?3)",
                params![body, proj, nows],
            );
        }
        self.input.clear();
        self.new_project.clear();
        self.use_new = false;
        self.reload();
    }

    fn use_prompt(&mut self, id: i64) {
        // find body
        let body = self.prompts.iter().find(|p| p.id == id).map(|p| compose(p));
        if let Some(text) = body {
            // clipboard
            match arboard::Clipboard::new() {
                Ok(mut cb) => {
                    if cb.set_text(text.clone()).is_ok() {
                        self.msg = format!("copied #{}", id);
                    } else {
                        self.msg = "clipboard failed".into();
                    }
                }
                Err(e) => self.msg = format!("clipboard: {}", e),
            }
        }
        if let Some(conn) = &self.conn {
            let nows = now();
            let _ = conn.execute(
                "UPDATE ideas SET status='done', snooze_until=NULL, updated_at=?1 WHERE id=?2",
                params![nows, id],
            );
        }
        self.reload();
    }
}

impl eframe::App for JotPanel {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Project:");
                egui::ComboBox::from_id_salt("proj_filter")
                    .selected_text(self.projects.get(self.selected).cloned().unwrap_or_else(|| "All".into()))
                    .show_ui(ui, |ui| {
                        for (i, p) in self.projects.iter().enumerate() {
                            ui.selectable_value(&mut self.selected, i, p);
                        }
                    });
                if ui.button("Refresh").clicked() {
                    self.reload();
                }
            });
        });

        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.label(&self.msg);
        });

        egui::Panel::left("capture").show(ui, |ui| {
            ui.heading("Capture");
            ui.text_edit_multiline(&mut self.input);
            ui.checkbox(&mut self.use_new, "new project");
            if self.use_new {
                ui.text_edit_singleline(&mut self.new_project);
            } else {
                ui.label(format!(
                    "project: {}",
                    self.projects.get(self.selected).cloned().unwrap_or_else(|| "All".into())
                ));
            }
            if ui.button("Add").clicked() {
                self.add();
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Prompts");
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut to_use: Option<i64> = None;
                for p in &self.prompts {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("#{}", p.id));
                            ui.strong(p.body.lines().next().unwrap_or("").to_string());
                        });
                        ui.label(format!(
                            "{} | prio {} | {}",
                            p.project.clone().unwrap_or_else(|| "-".into()),
                            p.priority,
                            p.tags.clone().unwrap_or_default()
                        ));
                        if ui.button("Use (copy)").clicked() {
                            to_use = Some(p.id);
                        }
                    });
                }
                if let Some(id) = to_use {
                    self.use_prompt(id);
                }
            });
        });

        if self.selected != self.last_selected {
            self.last_selected = self.selected;
            self.reload();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = db_path(cli.db);
    match cli.cmd {
        None => {
            println!("jot {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Commands::Path) => {
            println!("{}", path.display());
            Ok(())
        }
        Some(Commands::Add {
            text,
            kind,
            project,
            file,
            tags,
            priority,
            status,
            json,
        }) => {
            let conn = open_db(&path)?;
            let body = match text {
                Some(t) => t,
                None => {
                    let mut b = String::new();
                    io::stdin().read_to_string(&mut b)?;
                    b
                }
            };
            let kind = kind.unwrap_or_else(|| "idea".into());
            let status = status.unwrap_or_else(|| "inbox".into());
            let nows = now();
            conn.execute("INSERT INTO ideas (body,kind,status,project,file_path,tags,priority,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![body, kind, status, project, file, tags, priority.unwrap_or(0), nows.clone(), nows])?;
            let id = conn.last_insert_rowid();
            let idea = get_idea(&conn, id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&idea)?);
            } else {
                println!("{}", id);
            }
            Ok(())
        }
        Some(Commands::List {
            status,
            kind,
            project,
            tag,
            limit,
            json,
        }) => {
            let conn = open_db(&path)?;
            let mut q = format!("{} WHERE 1=1", SEL);
            let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![];
            if let Some(s) = &status {
                q.push_str(" AND status=?");
                p.push(Box::new(s.clone()));
            }
            if let Some(k) = &kind {
                q.push_str(" AND kind=?");
                p.push(Box::new(k.clone()));
            }
            if let Some(pr) = &project {
                q.push_str(" AND project=?");
                p.push(Box::new(pr.clone()));
            }
            if let Some(t) = &tag {
                q.push_str(" AND tags LIKE ?");
                p.push(Box::new(format!("%{}%", t)));
            }
            q.push_str(" ORDER BY id DESC");
            if let Some(l) = limit {
                q.push_str(&format!(" LIMIT {}", l));
            }
            let mut stmt = conn.prepare(&q)?;
            let params: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params.as_slice(), row_to_idea)?;
            let mut ideas: Vec<Idea> = vec![];
            for r in rows {
                ideas.push(r?);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&ideas)?);
            } else {
                for i in ideas {
                    println!(
                        "{} [{}] {} {}",
                        i.id,
                        i.kind,
                        i.status,
                        i.body
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(60)
                            .collect::<String>()
                    );
                }
            }
            Ok(())
        }
        Some(Commands::Search { query, json }) => {
            let conn = open_db(&path)?;
            // Escape FTS5 special chars by quoting the whole query as a phrase.
            let q = format!("\"{}\"", query.replace('"', "\"\""));
            let mut stmt = conn.prepare(&format!(
                "{} WHERE id IN (SELECT rowid FROM ideas_fts WHERE ideas_fts MATCH ?1)",
                SEL
            ))?;
            let rows = stmt.query_map(params![q], row_to_idea)?;
            let mut ideas: Vec<Idea> = vec![];
            for r in rows {
                ideas.push(r?);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&ideas)?);
            } else {
                for i in ideas {
                    println!("{} {}", i.id, i.body.lines().next().unwrap_or(""));
                }
            }
            Ok(())
        }
        Some(Commands::Show { id, json }) => {
            let conn = open_db(&path)?;
            let idea = get_idea(&conn, id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&idea)?);
            } else {
                println!("{}", compose(&idea));
            }
            Ok(())
        }
        Some(Commands::Set {
            id,
            status,
            snooze,
            priority,
            tags,
            project,
        }) => {
            let conn = open_db(&path)?;
            let mut idea = get_idea(&conn, id)?;
            if let Some(s) = status {
                let valid = [
                    "inbox",
                    "ready",
                    "waiting_agent",
                    "waiting_credits",
                    "sent",
                    "done",
                    "dropped",
                    "snoozed",
                ];
                if !valid.contains(&s.as_str()) {
                    anyhow::bail!("invalid status");
                }
                if s == "snoozed" {
                    let dur = snooze.clone().unwrap_or_else(|| "1h".into());
                    let until = chrono::Utc::now() + parse_dur(&dur)?;
                    idea.snooze_until = Some(until.to_rfc3339());
                } else {
                    idea.snooze_until = None;
                }
                idea.status = s;
            }
            if let Some(s) = snooze {
                if idea.status != "snoozed" {
                    idea.status = "snoozed".into();
                }
                let until = chrono::Utc::now() + parse_dur(&s)?;
                idea.snooze_until = Some(until.to_rfc3339());
            }
            if let Some(p) = priority {
                idea.priority = p;
            }
            if let Some(t) = tags {
                idea.tags = Some(t);
            }
            if let Some(pr) = project {
                idea.project = Some(pr);
            }
            let nows = now();
            conn.execute("UPDATE ideas SET body=?1,kind=?2,status=?3,project=?4,file_path=?5,tags=?6,priority=?7,agent_task_id=?8,snooze_until=?9,updated_at=?10 WHERE id=?11",
                params![idea.body, idea.kind, idea.status, idea.project, idea.file_path, idea.tags, idea.priority, idea.agent_task_id, idea.snooze_until, nows, id])?;
            Ok(())
        }
        Some(Commands::Done { id }) => {
            let conn = open_db(&path)?;
            let nows = now();
            conn.execute(
                "UPDATE ideas SET status='done', snooze_until=NULL, updated_at=?1 WHERE id=?2",
                params![nows, id],
            )?;
            Ok(())
        }
        Some(Commands::Drop { id }) => {
            let conn = open_db(&path)?;
            let nows = now();
            conn.execute(
                "UPDATE ideas SET status='dropped', snooze_until=NULL, updated_at=?1 WHERE id=?2",
                params![nows, id],
            )?;
            Ok(())
        }
        Some(Commands::Snooze { id, dur }) => {
            let conn = open_db(&path)?;
            let until = chrono::Utc::now() + parse_dur(&dur)?;
            let nows = now();
            conn.execute(
                "UPDATE ideas SET status='snoozed', snooze_until=?1, updated_at=?2 WHERE id=?3",
                params![until.to_rfc3339(), nows, id],
            )?;
            Ok(())
        }
        Some(Commands::Emit { id }) => {
            let conn = open_db(&path)?;
            let idea = get_idea(&conn, id)?;
            print!("{}", compose(&idea));
            Ok(())
        }
        Some(Commands::Copy { id }) => {
            let conn = open_db(&path)?;
            let idea = get_idea(&conn, id)?;
            let mut cb = arboard::Clipboard::new()?;
            cb.set_text(compose(&idea))?;
            Ok(())
        }
        Some(Commands::Promote { id, to }) => {
            let conn = open_db(&path)?;
            let idea = get_idea(&conn, id)?;
            let text = compose(&idea);
            // read config
            let cfg_path = dirs::config_dir()
                .unwrap_or_default()
                .join("jot")
                .join("config.toml");
            let cmd = to.or_else(|| {
                // parse promote.cmd
                std::fs::read_to_string(&cfg_path)
                    .ok()
                    .and_then(|s| {
                        // crude
                        s.lines().find(|l| l.trim().starts_with("cmd")).map(|l| {
                            l.split('=')
                                .nth(1)
                                .unwrap_or("")
                                .trim()
                                .trim_matches('"')
                                .to_string()
                        })
                    })
                    .filter(|s| !s.is_empty())
            });
            match cmd {
                None => {
                    print!("{}", text);
                    Ok(())
                }
                Some(c) => {
                    use std::process::{Command, Stdio};
                    let mut child = Command::new("sh")
                        .arg("-c")
                        .arg(&c)
                        .stdin(Stdio::piped())
                        .spawn()?;
                    use std::io::Write;
                    child.stdin.as_mut().unwrap().write_all(text.as_bytes())?;
                    drop(child.stdin.take());
                    let status = child.wait()?;
                    if status.success() {
                        let nows = now();
                        conn.execute("UPDATE ideas SET status='sent', snooze_until=NULL, updated_at=?1 WHERE id=?2", params![nows, id])?;
                        Ok(())
                    } else {
                        anyhow::bail!("command failed");
                    }
                }
            }
        }
        Some(Commands::Export { json: _ }) => {
            let conn = open_db(&path)?;
            let mut stmt = conn.prepare(&format!("{} ORDER BY id", SEL))?;
            let rows = stmt.query_map([], row_to_idea)?;
            let mut ideas: Vec<Idea> = vec![];
            for r in rows {
                ideas.push(r?);
            }
            println!("{}", serde_json::to_string_pretty(&ideas)?);
            Ok(())
        }
        Some(Commands::Import { file }) => {
            let conn = open_db(&path)?;
            let data = std::fs::read_to_string(&file)?;
            let ideas: Vec<Idea> = serde_json::from_str(&data)?;
            for i in ideas {
                conn.execute("INSERT INTO ideas (body,kind,status,project,file_path,tags,priority,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![i.body,i.kind,i.status,i.project,i.file_path,i.tags,i.priority,i.created_at,i.updated_at])?;
            }
            Ok(())
        }
        Some(Commands::Doctor) => {
            let conn = open_db(&path)?;
            let wal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            println!("db: {}", path.display());
            println!("wal: {}", wal);
            println!("hotkey: n/a (panel not running)");
            println!("tray: n/a");
            Ok(())
        }
        Some(Commands::Panel) => {
            let opts = eframe::NativeOptions::default();
            let p = path.clone();
            eframe::run_native(
                "jot",
                opts,
                Box::new(move |_cc| Ok(Box::new(JotPanel::new(p)))),
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(())
        }
        Some(Commands::Edit { id }) => {
            let conn = open_db(&path)?;
            let idea = get_idea(&conn, id)?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let tmp = std::env::temp_dir().join(format!("jot-{}-{}.md", id, std::process::id()));
            std::fs::write(&tmp, &idea.body)?;
            std::process::Command::new(&editor).arg(&tmp).status()?;
            let newbody = std::fs::read_to_string(&tmp)?;
            let nows = now();
            conn.execute(
                "UPDATE ideas SET body=?1, updated_at=?2 WHERE id=?3",
                params![newbody, nows, id],
            )?;
            let _ = std::fs::remove_file(&tmp);
            Ok(())
        }
    }
}
