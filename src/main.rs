// baluchon – Gestionnaire d'applications portables
// Fonctionne sous Linux et Windows à partir d'une clé USB

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(not(target_os = "windows"))]
extern crate libc;

// ── Modèle de données ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppEntry {
    name: String,
    #[serde(default)]
    exec_linux: Option<String>,
    #[serde(default)]
    exec_windows: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    /// Script à lancer par le bouton "Initialiser" (ex: "transcribe/setupPython_and_download.sh")
    #[serde(default)]
    setup_script: Option<String>,
}

impl AppEntry {
    fn exec_path(&self) -> Option<&str> {
        #[cfg(target_os = "windows")]
        return self.exec_windows.as_deref();
        #[cfg(not(target_os = "windows"))]
        return self.exec_linux.as_deref();
    }
}

// ── Panneau install-assistant ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum FileStatus {
    Pending,
    Downloading { percent: f32 },
    Done { mib: f32, secs: u32 },
    Failed(String),
}

#[derive(Debug, Clone)]
struct DownloadFile {
    name: String,
    dest: String,
    size_mib: Option<f32>,
    status: FileStatus,
}

#[derive(Debug, Clone)]
struct InstallState {
    files: Vec<DownloadFile>,
    current_idx: usize,          // fichier en cours (global)
    current_file_pct: f32,       // progression du fichier courant 0..1
    log: Vec<String>,
    done: bool,
    error: Option<String>,
    /// Secondes avant fermeture auto (None = pas encore terminé)
    auto_close_secs: Option<u32>,
    auto_close_start: Option<Instant>,
}

impl InstallState {
    fn new() -> Self {
        Self {
            files: vec![],
            current_idx: 0,
            current_file_pct: 0.0,
            log: vec![],
            done: false,
            error: None,
            auto_close_secs: None,
            auto_close_start: None,
        }
    }

    fn done_count(&self) -> usize {
        self.files.iter().filter(|f| matches!(f.status, FileStatus::Done { .. })).count()
    }

    fn fail_count(&self) -> usize {
        self.files.iter().filter(|f| matches!(f.status, FileStatus::Failed(_))).count()
    }

    fn global_pct(&self) -> f32 {
        let n = self.files.len();
        if n == 0 {
            return 0.0;
        }
        let done = self.done_count() as f32;
        let partial = self.current_file_pct / n as f32;
        (done / n as f32 + partial).min(1.0)
    }
}

type SharedState = Arc<Mutex<InstallState>>;

// Extrait les infos de progression depuis la sortie de install-assistant.
// Format attendu (flexible) :
//   FILE <nom> <dest> [<taille_MiB>]
//   PROGRESS <pct_float>
//   DONE <nom> <taille_MiB> <secs>
//   FAIL <nom> <message>
//   LOG <message libre>
fn parse_line(line: &str, state: &mut InstallState) {
    let now = chrono_stamp();
    let parts: Vec<&str> = line.splitn(5, ' ').collect();
    match parts.as_slice() {
        ["FILE", name, dest, rest @ ..] => {
            let size_mib = rest.first().and_then(|s| s.parse::<f32>().ok());
            state.files.push(DownloadFile {
                name: name.to_string(),
                dest: dest.to_string(),
                size_mib,
                status: FileStatus::Pending,
            });
        }
        ["PROGRESS", pct_s] => {
            let pct: f32 = pct_s.parse().unwrap_or(0.0);
            state.current_file_pct = pct / 100.0;
            // Marque le fichier courant comme en cours
            if let Some(f) = state.files.get_mut(state.current_idx) {
                f.status = FileStatus::Downloading { percent: pct };
            }
        }
        ["DONE", name, mib_s, secs_s] => {
            let mib: f32 = mib_s.parse().unwrap_or(0.0);
            let secs: u32 = secs_s.parse().unwrap_or(0);
            if let Some(f) = state.files.iter_mut().find(|f| f.name == *name) {
                f.status = FileStatus::Done { mib, secs };
            }
            state.current_idx += 1;
            state.current_file_pct = 0.0;
            state.log.push(format!("[{}] ✅ OK ({:.0} MiB en {}s)", now, mib, secs));
        }
        ["FAIL", name, rest @ ..] => {
            let msg = rest.join(" ");
            if let Some(f) = state.files.iter_mut().find(|f| f.name == *name) {
                f.status = FileStatus::Failed(msg.clone());
            }
            state.current_idx += 1;
            state.current_file_pct = 0.0;
            state.log.push(format!("[{}] ❌ FAIL {} : {}", now, name, msg));
        }
        _ => {
            // Ligne brute → log
            if !line.trim().is_empty() {
                state.log.push(format!("[{}] {}", now, line));
            }
        }
    }
}

fn chrono_stamp() -> String {
    // Heure locale approximative via SystemTime
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Lance le setup script en arrière-plan et alimente SharedState
/// Lance un binaire en capturant stdout+stderr via des fichiers temporaires
/// (évite ENXIO / os error 6 sur les systèmes de fichiers FAT/exFAT des clés USB
/// qui ne supportent pas les pipes anonymes).
/// Lit les nouvelles lignes depuis `path` à partir de `offset` octets.
/// Retourne le nouvel offset.
fn read_new_lines(
    path: &Path,
    offset: u64,
    _label: &str,
    parse: bool,
    state: &SharedState,
    ctx: &egui::Context,
) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return offset,
    };
    if f.seek(SeekFrom::Start(offset)).is_err() { return offset; }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() { return offset; }
    let new_offset = offset + buf.len() as u64;
    for line in buf.lines() {
        if line.trim().is_empty() { continue; }
        let mut st = state.lock().unwrap();
        if parse {
            parse_line(line, &mut st);
        } else {
            st.log.push(format!("[{}] {}", chrono_stamp(), line));
        }
        ctx.request_repaint();
    }
    new_offset
}

/// Retourne un répertoire temporaire garanti sur le système de fichiers local
/// (jamais sur la clé USB, évite ENXIO sur FAT/exFAT).
fn local_tmp() -> PathBuf {
    // Priorité : /tmp (tmpfs, toujours local sous Linux/macOS)
    // puis TMPDIR/TMP/TEMP, puis le répertoire courant en dernier recours.
    #[cfg(not(target_os = "windows"))]
    {
        let p = PathBuf::from("/tmp");
        if p.exists() { return p; }
    }
    // std::env::temp_dir() peut pointer vers la clé sur certaines configs —
    // on le garde en dernier recours seulement.
    std::env::temp_dir()
}

/// Variante de spawn_subprocess acceptant des args Vec<String>.
/// Toutes les redirections utilisent des fichiers dans /tmp (local) pour
/// éviter ENXIO (os error 6) sur les clés USB FAT/exFAT.
fn spawn_subprocess_with_args(
    binary: PathBuf,
    args: Vec<String>,
    label: &'static str,
    state: SharedState,
    ctx: egui::Context,
    parse: bool,
) {
    std::thread::spawn(move || {
        {
            let mut st = state.lock().unwrap();
            st.log.push(format!("[{}] ▶ {}", chrono_stamp(),
                if args.is_empty() {
                    binary.display().to_string()
                } else {
                    format!("{} {}", binary.display(), args.join(" "))
                }));
        }

        // Fichiers temporaires dans /tmp (système local, jamais sur la clé)
        let tmp_base = local_tmp();
        let tmp_out = tmp_base.join(format!("baluchon_{}_out.txt", label));
        let tmp_err = tmp_base.join(format!("baluchon_{}_err.txt", label));

        // stdout → fichier tmp local
        let f_out = match std::fs::File::create(&tmp_out) {
            Ok(f) => f,
            Err(e) => {
                let mut st = state.lock().unwrap();
                st.error = Some(format!("Impossible de créer {}: {}", tmp_out.display(), e));
                st.done = true;
                ctx.request_repaint();
                return;
            }
        };

        // stderr → fichier tmp local (Stdio::null() en dernier recours)
        let stderr_stdio = match std::fs::File::create(&tmp_err) {
            Ok(f)  => Stdio::from(f),
            Err(_) => Stdio::null(),   // null() est géré en interne par std, sans ouvrir de fichier
        };

        let mut cmd = Command::new(&binary);
        for a in &args { cmd.arg(a); }

        // current_dir sur le répertoire parent du binaire réel (bash → /bin, pas la clé)
        // On utilise plutôt le répertoire de l'argument principal quand on passe via bash/cmd.
        // Pour un script "bash -c /chemin/script.sh", on set le workdir sur le dossier du script.
        let workdir: Option<PathBuf> = {
            // Si binary est bash/cmd, extraire le chemin réel depuis les args
            let bin_name = binary.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if bin_name == "bash" || bin_name == "cmd" {
                // Le script est le dernier argument
                args.last()
                    .map(|s| PathBuf::from(s.trim_start_matches("-c").trim()))
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                    .filter(|d| d.exists())
            } else {
                binary.parent().map(|d| d.to_path_buf())
            }
        };
        if let Some(ref dir) = workdir {
            cmd.current_dir(dir);
        }

        // setsid() : nouvelle session sans TTY de contrôle (évite ENXIO)
        #[cfg(not(target_os = "windows"))]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let result = cmd
            .stdout(Stdio::from(f_out))
            .stderr(stderr_stdio)
            .stdin(Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                // Diagnostic spécifique pour ENXIO (os error 6)
                let hint = if e.raw_os_error() == Some(6) {
                    " (ENXIO : le binaire tente d'accéder à /dev/tty — essayez de le lancer depuis un terminal)"
                } else {
                    ""
                };
                let mut st = state.lock().unwrap();
                st.error = Some(format!("Lancement impossible : {}{}", e, hint));
                st.done = true;
                ctx.request_repaint();
                return;
            }
        };

        // Lecture incrémentale de stdout via le fichier tmp
        let mut offset: u64 = 0;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None)    => {}
                Err(_)      => break,
            }
            offset = read_new_lines(&tmp_out, offset, label, parse, &state, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        read_new_lines(&tmp_out, offset, label, parse, &state, &ctx);

        // Récupération stderr
        if let Ok(content) = std::fs::read_to_string(&tmp_err) {
            for line in content.lines() {
                let t = line.trim();
                if t.is_empty() { continue; }
                let mut st = state.lock().unwrap();
                // ENXIO résiduel : le binaire accède à /dev/tty malgré setsid.
                // On l'affiche en ℹ plutôt qu'en ⚠ pour ne pas alarmer.
                if t.contains("os error 6") || t.contains("ENXIO")
                    || t.contains("No such device or address")
                {
                    st.log.push(format!(
                        "[{}] ℹ Le programme a besoin d'un terminal (PTY). \
                         Relancez-le depuis un terminal si l'affichage est vide.",
                        chrono_stamp()));
                } else {
                    st.log.push(format!("[{}] ⚠ {}", chrono_stamp(), t));
                }
            }
        }
        let _ = std::fs::remove_file(&tmp_out);
        let _ = std::fs::remove_file(&tmp_err);

        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        {
            let mut st = state.lock().unwrap();
            st.done = true;
            st.log.push(format!("[{}] {} {} terminé",
                chrono_stamp(), if ok { "✅" } else { "❌" }, label));
            st.auto_close_secs  = Some(5);
            st.auto_close_start = Some(Instant::now());
        }
        ctx.request_repaint();
    });
}


fn spawn_setup_init(binary: PathBuf, state: SharedState, ctx: egui::Context) {
    #[cfg(not(target_os = "windows"))]
    {
        let bin_str = binary.to_string_lossy().to_string();

        // Les binaires TUI Rust (crossterm, ratatui…) ouvrent /dev/tty directement.
        // bash -c seul ne suffit pas : il faut un vrai pseudo-terminal (PTY).
        // `script -q -c CMD /dev/null` alloue un PTY et redirige stdout vers nous.
        // Si `script` est absent (rare), on repasse sur bash -c.
        let script_bin = PathBuf::from("/usr/bin/script");
        let (launcher, args) = if script_bin.exists() {
            (
                script_bin,
                vec![
                    "-q".to_string(),           // silencieux (pas de header/trailer)
                    "-c".to_string(),
                    bin_str,
                    "/dev/null".to_string(),    // fichier de log script → /dev/null
                ],
            )
        } else {
            (
                PathBuf::from("/bin/bash"),
                vec!["-c".to_string(), bin_str],
            )
        };

        spawn_subprocess_with_args(launcher, args, "setup-init", state, ctx, false);
    }
    #[cfg(target_os = "windows")]
    {
        let bin_str = binary.to_string_lossy().to_string();
        spawn_subprocess_with_args(
            PathBuf::from("cmd"),
            vec!["/c".to_string(), bin_str],
            "setup-init", state, ctx, false,
        );
    }
}

// ── Internationalisation ───────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum Lang {
    Fr,
    En,
}

struct Strings {
    lang: Lang,
}

impl Strings {
    fn new() -> Self {
        let lang_env = std::env::var("LANG")
            .or_else(|_| std::env::var("LANGUAGE"))
            .unwrap_or_default();
        let lang = if lang_env.starts_with("fr") { Lang::Fr } else { Lang::En };
        Self { lang }
    }

    fn title(&self) -> &str {
        match self.lang {
            Lang::Fr => "Baluchon – Applis portables",
            Lang::En => "Baluchon – Portable Apps",
        }
    }
    fn btn_launch(&self) -> &str {
        match self.lang {
            Lang::Fr => "▶  Lancer",
            Lang::En => "▶  Launch",
        }
    }
    fn btn_install(&self) -> &str {
        match self.lang {
            Lang::Fr => "📌  Installer les raccourcis",
            Lang::En => "📌  Install shortcuts",
        }
    }
    fn btn_remove(&self) -> &str {
        match self.lang {
            Lang::Fr => "🗑  Supprimer les raccourcis",
            Lang::En => "🗑  Remove shortcuts",
        }
    }
    fn btn_download(&self) -> &str {
        match self.lang {
            Lang::Fr => "⚙  Initialiser",
            Lang::En => "⚙  Initialize",
        }
    }
    fn msg_no_setup_script(&self) -> String {
        match self.lang {
            Lang::Fr => "❌ Aucun setup_script défini pour cette app".to_string(),
            Lang::En => "❌ No setup_script defined for this app".to_string(),
        }
    }
    fn msg_setup_missing(&self, p: &str) -> String {
        match self.lang {
            Lang::Fr => format!("❌ Script introuvable : {}", p),
            Lang::En => format!("❌ Script not found: {}", p),
        }
    }
    fn label_usb(&self) -> &str {
        match self.lang {
            Lang::Fr => "Clé USB détectée :",
            Lang::En => "USB drive detected:",
        }
    }
    fn label_no_usb(&self) -> &str {
        match self.lang {
            Lang::Fr => "❌ Aucune clé USB trouvée",
            Lang::En => "❌ No USB drive found",
        }
    }
    fn label_no_apps(&self) -> &str {
        match self.lang {
            Lang::Fr => "Aucune application trouvée dans apps.json",
            Lang::En => "No apps found in apps.json",
        }
    }
    fn label_status(&self) -> &str {
        match self.lang {
            Lang::Fr => "Statut",
            Lang::En => "Status",
        }
    }
    fn label_lang(&self) -> &str {
        "Langue / Language"
    }
    fn msg_launched(&self, name: &str) -> String {
        match self.lang {
            Lang::Fr => format!("✅ {} lancé", name),
            Lang::En => format!("✅ {} launched", name),
        }
    }
    fn msg_shortcut_ok(&self, name: &str) -> String {
        match self.lang {
            Lang::Fr => format!("✅ Raccourci installé pour {}", name),
            Lang::En => format!("✅ Shortcut installed for {}", name),
        }
    }
    fn msg_shortcut_removed(&self, name: &str) -> String {
        match self.lang {
            Lang::Fr => format!("🗑 Raccourci supprimé pour {}", name),
            Lang::En => format!("🗑 Shortcut removed for {}", name),
        }
    }
    fn msg_error(&self, e: &str) -> String {
        match self.lang {
            Lang::Fr => format!("❌ Erreur : {}", e),
            Lang::En => format!("❌ Error: {}", e),
        }
    }
    fn msg_no_exec(&self) -> String {
        match self.lang {
            Lang::Fr => "❌ Pas d'exécutable défini pour cet OS".to_string(),
            Lang::En => "❌ No executable defined for this OS".to_string(),
        }
    }
    fn ia_title(&self) -> &str {
        match self.lang {
            Lang::Fr => "setup-assistant",
            Lang::En => "setup-assistant",
        }
    }
    fn ia_btn_close(&self) -> &str {
        match self.lang {
            Lang::Fr => "[q] Fermer",
            Lang::En => "[q] Close",
        }
    }
    fn ia_auto_close(&self, n: u32) -> String {
        match self.lang {
            Lang::Fr => format!("fermeture auto dans {}s", n),
            Lang::En => format!("auto-close in {}s", n),
        }
    }
    fn ia_in_progress(&self) -> &str {
        match self.lang {
            Lang::Fr => "En cours",
            Lang::En => "In progress",
        }
    }
    fn ia_file_col(&self) -> &str {
        match self.lang {
            Lang::Fr => "Fichier",
            Lang::En => "File",
        }
    }
    fn ia_dest_col(&self) -> &str {
        match self.lang {
            Lang::Fr => "Destination",
            Lang::En => "Destination",
        }
    }
    fn ia_size_col(&self) -> &str {
        match self.lang {
            Lang::Fr => "Taille",
            Lang::En => "Size",
        }
    }
    fn ia_stat_col(&self) -> &str {
        match self.lang {
            Lang::Fr => "Statut",
            Lang::En => "Status",
        }
    }
    fn ia_searching(&self) -> &str {
        match self.lang {
            Lang::Fr => "Recherche de l’assistant de configuration…",
            Lang::En => "Looking for setup assistant…",
        }
    }
    fn py_title(&self) -> &str {
        "setup_venv_lang.sh"
    }
}

// ── Détection de la clé USB ────────────────────────────────────────────────────

fn find_usb_root() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.clone();
        for _ in 0..4 {
            p.pop();
            if p.join("apps.json").exists() {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for base in &["/media", "/run/media", "/mnt"] {
            if let Ok(user) = std::env::var("USER") {
                let user_media = Path::new(base).join(&user);
                if let Ok(entries) = std::fs::read_dir(&user_media) {
                    for entry in entries.flatten() {
                        if entry.path().join("apps.json").exists() {
                            return Some(entry.path());
                        }
                    }
                }
            }
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    if entry.path().join("apps.json").exists() {
                        return Some(entry.path());
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        for letter in b'D'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if Path::new(&drive).join("apps.json").exists() {
                return Some(PathBuf::from(&drive));
            }
        }
    }

    None
}

fn load_apps(usb_root: &Path) -> Result<Vec<AppEntry>, String> {
    let path = usb_root.join("apps.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {}", path.display(), e))?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

// ── Raccourcis Linux ──────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
fn detect_desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(not(target_os = "windows"))]
fn install_shortcut_linux(app: &AppEntry, usb_root: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;

    let exec_rel = app.exec_linux.as_deref().ok_or("Pas d'exec_linux")?;
    let exec_abs = usb_root.join(exec_rel);

    if exec_abs.exists() {
        let mut perms = std::fs::metadata(&exec_abs).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exec_abs, perms).map_err(|e| e.to_string())?;
    }

    let apps_dir = dirs_next::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("applications");
    std::fs::create_dir_all(&apps_dir).map_err(|e| e.to_string())?;

    let icon_path = app
        .icon
        .as_ref()
        .map(|i| usb_root.join(i).to_string_lossy().to_string())
        .unwrap_or_default();

    let desktop_content = format!(
        "[Desktop Entry]\nVersion=1.0\nType=Application\nName={name}\n\
         Comment={comment}\nExec={exec}\nIcon={icon}\nTerminal=false\nCategories=Utility;\n",
        name = app.name,
        comment = app.description.as_deref().unwrap_or(""),
        exec = exec_abs.to_string_lossy(),
        icon = icon_path,
    );

    let desktop_file = apps_dir.join(format!("{}.desktop", app.name.to_lowercase().replace(' ', "_")));
    std::fs::write(&desktop_file, &desktop_content).map_err(|e| e.to_string())?;

    let mut perms = std::fs::metadata(&desktop_file).map_err(|e| e.to_string())?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&desktop_file, perms).map_err(|e| e.to_string())?;

    let _ = Command::new("update-desktop-database").arg(&apps_dir).spawn();

    let desktop_env = detect_desktop().to_lowercase();
    if desktop_env.contains("gnome") || desktop_env.contains("kde")
        || desktop_env.contains("cinnamon") || desktop_env.contains("xfce")
    {
        if let Some(user_desktop) = dirs_next::desktop_dir() {
            let dest = user_desktop.join(format!("{}.desktop", app.name.to_lowercase().replace(' ', "_")));
            let _ = std::fs::copy(&desktop_file, &dest);
            if let Ok(m) = std::fs::metadata(&dest) {
                let mut p = m.permissions();
                p.set_mode(0o755);
                let _ = std::fs::set_permissions(&dest, p);
            }
        }
    }
    Ok(desktop_file.to_string_lossy().to_string())
}

#[cfg(not(target_os = "windows"))]
fn remove_shortcut_linux(app: &AppEntry) -> Result<(), String> {
    let file_name = format!("{}.desktop", app.name.to_lowercase().replace(' ', "_"));
    if let Some(apps_dir) = dirs_next::data_local_dir().map(|d| d.join("applications")) {
        let p = apps_dir.join(&file_name);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| e.to_string())?;
        }
    }
    if let Some(desktop) = dirs_next::desktop_dir() {
        let p = desktop.join(&file_name);
        if p.exists() {
            let _ = std::fs::remove_file(&p);
        }
    }
    Ok(())
}

// ── Raccourcis Windows ────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn install_shortcut_windows(app: &AppEntry, usb_root: &Path) -> Result<String, String> {
    let exec_rel = app.exec_windows.as_deref().ok_or("Pas d'exec_windows")?;
    let exec_abs = usb_root.join(exec_rel);
    let start_menu = dirs_next::data_local_dir()
        .map(|d| d.join("Microsoft\\Windows\\Start Menu\\Programs"))
        .ok_or("Impossible de trouver le menu Démarrer")?;
    std::fs::create_dir_all(&start_menu).map_err(|e| e.to_string())?;
    let lnk_path = start_menu.join(format!("{}.lnk", app.name));
    let ps_script = format!(
        r#"$WS = New-Object -ComObject WScript.Shell
$SC = $WS.CreateShortcut('{lnk}')
$SC.TargetPath = '{target}'
$SC.WorkingDirectory = '{dir}'
$SC.Description = '{desc}'
$SC.Save()"#,
        lnk = lnk_path.to_string_lossy().replace('\'', "''"),
        target = exec_abs.to_string_lossy().replace('\'', "''"),
        dir = usb_root.to_string_lossy().replace('\'', "''"),
        desc = app.description.as_deref().unwrap_or("").replace('\'', "''"),
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_script])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(lnk_path.to_string_lossy().to_string())
    } else {
        Err("Échec du script PowerShell".to_string())
    }
}

#[cfg(target_os = "windows")]
fn remove_shortcut_windows(app: &AppEntry) -> Result<(), String> {
    let start_menu = dirs_next::data_local_dir()
        .map(|d| d.join("Microsoft\\Windows\\Start Menu\\Programs"))
        .ok_or("Menu Démarrer introuvable")?;
    let lnk = start_menu.join(format!("{}.lnk", app.name));
    if lnk.exists() {
        std::fs::remove_file(&lnk).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Actions génériques ─────────────────────────────────────────────────────────

fn launch_app(app: &AppEntry, usb_root: &Path) -> Result<(), String> {
    let rel = app.exec_path().ok_or("Pas d'exécutable pour cet OS")?;
    let abs = usb_root.join(rel);
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(&abs) {
            let mut p = m.permissions();
            p.set_mode(0o755);
            let _ = std::fs::set_permissions(&abs, p);
        }
    }
    Command::new(&abs).spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn install_shortcut(app: &AppEntry, usb_root: &Path) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    return install_shortcut_windows(app, usb_root);
    #[cfg(not(target_os = "windows"))]
    return install_shortcut_linux(app, usb_root);
}

fn remove_shortcut(app: &AppEntry) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    return remove_shortcut_windows(app);
    #[cfg(not(target_os = "windows"))]
    return remove_shortcut_linux(app);
}

// ── Interface graphique (egui) ─────────────────────────────────────────────────

struct BaluchonApp {
    usb_root: Option<PathBuf>,
    apps: Vec<AppEntry>,
    selected: Option<usize>,
    status: String,
    strings: Strings,
    load_error: Option<String>,
    /// Panneau install-assistant (None = fermé)
    install_panel: Option<SharedState>,
    /// Panneau setup init (None = fermé)
    setup_panel: Option<SharedState>,
    /// Logo baluchon.png
    logo: Option<egui::TextureHandle>,
}

impl BaluchonApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.text_styles.insert(egui::TextStyle::Body,    egui::FontId::proportional(15.0));
        style.text_styles.insert(egui::TextStyle::Button,  egui::FontId::proportional(15.0));
        style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
        cc.egui_ctx.set_style(style);

        let strings   = Strings::new();
        let usb_root  = find_usb_root();
        let (apps, load_error) = match &usb_root {
            Some(root) => match load_apps(root) {
                Ok(a)  => (a, None),
                Err(e) => (vec![], Some(e)),
            },
            None => (vec![], None),
        };

        Self { usb_root, apps, selected: None, status: String::new(),
               strings, load_error, install_panel: None, setup_panel: None, logo: None }
    }

    fn selected_app(&self) -> Option<&AppEntry> {
        self.selected.and_then(|i| self.apps.get(i))
    }

    fn action_launch(&mut self) {
        if let (Some(app), Some(root)) = (self.selected_app().cloned(), &self.usb_root.clone()) {
            match launch_app(&app, root) {
                Ok(_)  => self.status = self.strings.msg_launched(&app.name),
                Err(e) => self.status = self.strings.msg_error(&e),
            }
        } else {
            self.status = self.strings.msg_no_exec();
        }
    }

    fn action_install(&mut self) {
        if let (Some(app), Some(root)) = (self.selected_app().cloned(), &self.usb_root.clone()) {
            match install_shortcut(&app, root) {
                Ok(_)  => self.status = self.strings.msg_shortcut_ok(&app.name),
                Err(e) => self.status = self.strings.msg_error(&e),
            }
        }
    }

    fn action_remove(&mut self) {
        if let Some(app) = self.selected_app().cloned() {
            match remove_shortcut(&app) {
                Ok(_)  => self.status = self.strings.msg_shortcut_removed(&app.name),
                Err(e) => self.status = self.strings.msg_error(&e),
            }
        }
    }

    /// Bouton "Initialiser" : lance le setup_script défini dans apps.json pour l'app sélectionnée
    fn action_download(&mut self, ctx: &egui::Context) {
        let root = match &self.usb_root {
            Some(r) => r.clone(),
            None    => { self.status = self.strings.msg_no_exec(); return; }
        };

        // Récupère le setup_script de l'app sélectionnée
        let script_rel = match self.selected_app().and_then(|a| a.setup_script.as_deref()) {
            Some(s) => s.to_owned(),
            None    => { self.status = self.strings.msg_no_setup_script(); return; }
        };

        // Chemin absolu
        let script = root.join(&script_rel);
        if !script.exists() {
            self.status = self.strings.msg_setup_missing(&script.to_string_lossy());
            return;
        }

        // chmod +x sur Linux
        #[cfg(not(target_os = "windows"))]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(&script) {
                let mut p = m.permissions();
                p.set_mode(0o755);
                let _ = std::fs::set_permissions(&script, p);
            }
        }

        let shared: SharedState = Arc::new(Mutex::new(InstallState::new()));
        spawn_setup_init(script, Arc::clone(&shared), ctx.clone());
        self.install_panel = Some(shared);
    }

    // ── Rendu du panneau install-assistant ────────────────────────────────────

    fn draw_install_panel(&mut self, ctx: &egui::Context) -> bool {
        // Retourne true si le panneau doit rester ouvert
        let shared = match &self.install_panel {
            Some(s) => Arc::clone(s),
            None    => return false,
        };

        // Couleurs
        let c_bg      = egui::Color32::from_rgb(0x0D, 0x0F, 0x1A);
        let c_border  = egui::Color32::from_rgb(0x3D, 0x8B, 0xFF);
        let c_dim     = egui::Color32::from_rgb(0x66, 0x6E, 0x88);
        let c_text    = egui::Color32::from_rgb(0xE0, 0xE4, 0xF0);
        let c_ok      = egui::Color32::from_rgb(0x4C, 0xD9, 0x7A);
        let c_err     = egui::Color32::from_rgb(0xFF, 0x5A, 0x5A);
        let c_accent  = egui::Color32::from_rgb(0x3D, 0x8B, 0xFF);
        let c_bar_bg  = egui::Color32::from_rgb(0x20, 0x24, 0x38);
        let c_bar_ok  = egui::Color32::from_rgb(0x4C, 0xD9, 0x7A);
        let c_bar_cur = egui::Color32::from_rgb(0x3D, 0x8B, 0xFF);

        let mut keep_open = true;

        egui::Window::new(self.strings.ia_title())
            .collapsible(false)
            .resizable(true)
            .default_size([620.0, 480.0])
            .min_size([500.0, 380.0])
            .frame(
                egui::Frame::none()
                    .fill(c_bg)
                    .stroke(egui::Stroke::new(1.5, c_border))
                    .rounding(6.0)
                    .inner_margin(14.0),
            )
            .show(ctx, |ui| {
                let st = shared.lock().unwrap();

                // ── Section 1 : barre globale ──────────────────────────────
                let n       = st.files.len();
                let done    = st.done_count();
                let fails   = st.fail_count();
                let global  = st.global_pct();
                let bar_w   = ui.available_width() - 4.0;

                ui.horizontal(|ui| {
                    // barre pleine largeur
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(bar_w, 22.0), egui::Sense::hover());
                    let filled_w = rect.width() * global;
                    ui.painter().rect_filled(rect, 4.0, c_bar_bg);
                    if filled_w > 0.0 {
                        let fill_rect = egui::Rect::from_min_size(
                            rect.min, egui::vec2(filled_w, rect.height()));
                        ui.painter().rect_filled(fill_rect, 4.0, c_bar_ok);
                    }
                    // Texte centré sur la barre
                    let label = if n > 0 {
                        format!("{}/{}  {:.0}%   ✅ {}  ❌ {}", done, n, global * 100.0, done, fails)
                    } else {
                        self.strings.ia_searching().to_string()
                    };
                    ui.painter().text(
                        rect.center(), egui::Align2::CENTER_CENTER,
                        &label,
                        egui::FontId::monospace(13.0),
                        c_text,
                    );
                });

                ui.add_space(6.0);
                ui.separator();

                // ── Section 2 : barre du fichier courant ──────────────────
                if n > 0 {
                    let cur_pct = st.current_file_pct;
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.colored_label(c_dim, format!("  {}  ", self.strings.ia_in_progress()));
                        let avail = ui.available_width() - 8.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(avail, 18.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 3.0, c_bar_bg);
                        let fw = rect.width() * cur_pct;
                        if fw > 0.0 {
                            let fr = egui::Rect::from_min_size(rect.min, egui::vec2(fw, rect.height()));
                            ui.painter().rect_filled(fr, 3.0, c_bar_cur);
                        }
                        ui.painter().text(
                            rect.center(), egui::Align2::CENTER_CENTER,
                            format!("{:.0}%", cur_pct * 100.0),
                            egui::FontId::monospace(12.0),
                            c_text,
                        );
                    });
                    ui.add_space(4.0);
                    ui.separator();
                }

                // ── Section 3 : tableau des fichiers ──────────────────────
                if !st.files.is_empty() {
                    ui.add_space(4.0);
                    egui::Grid::new("files_grid")
                        .striped(true)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            // En-têtes
                            ui.colored_label(c_dim, " # ");
                            ui.colored_label(c_dim, self.strings.ia_file_col());
                            ui.colored_label(c_dim, self.strings.ia_dest_col());
                            ui.colored_label(c_dim, self.strings.ia_size_col());
                            ui.colored_label(c_dim, self.strings.ia_stat_col());
                            ui.end_row();

                            for (idx, f) in st.files.iter().enumerate() {
                                let num_icon = match &f.status {
                                    FileStatus::Done { .. }       => "✓",
                                    FileStatus::Downloading { .. } => "▶",
                                    FileStatus::Failed(_)         => "✗",
                                    FileStatus::Pending           => " ",
                                };
                                let row_color = match &f.status {
                                    FileStatus::Done { .. }  => c_ok,
                                    FileStatus::Failed(_)    => c_err,
                                    FileStatus::Downloading { .. } => c_accent,
                                    FileStatus::Pending      => c_dim,
                                };

                                ui.colored_label(row_color, format!("{}{}", num_icon, idx + 1));

                                // Nom tronqué
                                let name_short = if f.name.len() > 18 {
                                    format!("{}…", &f.name[..17])
                                } else {
                                    f.name.clone()
                                };
                                ui.colored_label(c_text, name_short);

                                // Dest tronquée
                                let dest_short = if f.dest.len() > 12 {
                                    format!("{}…", &f.dest[..11])
                                } else {
                                    f.dest.clone()
                                };
                                ui.colored_label(c_dim, dest_short);

                                // Taille
                                let size_str = match &f.status {
                                    FileStatus::Done { mib, .. } => format!("{:.0} MiB", mib),
                                    _ => f.size_mib.map(|m| format!("{:.1} MiB", m))
                                              .unwrap_or_else(|| "-".to_string()),
                                };
                                ui.colored_label(c_dim, size_str);

                                // Statut
                                let stat_str = match &f.status {
                                    FileStatus::Pending              => "⏳".to_string(),
                                    FileStatus::Downloading { percent } => format!("⬇ {:.0}%", percent),
                                    FileStatus::Done { secs, .. }    => format!("✅ {}s", secs),
                                    FileStatus::Failed(msg)          => format!("❌ {}", msg),
                                };
                                ui.colored_label(row_color, stat_str);
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.separator();
                }

                // ── Section 4 : log ────────────────────────────────────────
                egui::ScrollArea::vertical()
                    .max_height(120.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &st.log {
                            ui.colored_label(c_dim, line);
                        }
                    });

                ui.separator();

                // ── Section 5 : barre de commandes ────────────────────────
                ui.horizontal(|ui| {
                    let close_btn = egui::Button::new(
                        egui::RichText::new(self.strings.ia_btn_close()).color(c_text))
                        .fill(egui::Color32::from_rgb(0x28, 0x2C, 0x42));
                    if ui.add(close_btn).clicked() {
                        keep_open = false;
                    }

                    // Fermeture automatique
                    if let (Some(delay), Some(start)) = (st.auto_close_secs, st.auto_close_start) {
                        let elapsed = start.elapsed().as_secs() as u32;
                        let remaining = delay.saturating_sub(elapsed);
                        ui.colored_label(c_dim, format!("·  {}", self.strings.ia_auto_close(remaining)));
                        if remaining == 0 {
                            keep_open = false;
                        }
                        // Rafraîchit chaque seconde
                        ctx.request_repaint_after(std::time::Duration::from_millis(500));
                    }

                    if let Some(ref err) = st.error {
                        ui.colored_label(c_err, err);
                    }
                });
            });

        keep_open
    }

    /// Panneau de suivi pour setup_venv_lang.sh (log brut)
    fn draw_setup_panel(&mut self, ctx: &egui::Context) -> bool {
        let shared = match &self.setup_panel {
            Some(s) => Arc::clone(s),
            None    => return false,
        };

        let c_bg     = egui::Color32::from_rgb(0x0D, 0x12, 0x0D);
        let c_border = egui::Color32::from_rgb(0x4C, 0xD9, 0x7A);
        let c_dim    = egui::Color32::from_rgb(0x66, 0x6E, 0x88);
        let c_text   = egui::Color32::from_rgb(0xE0, 0xE4, 0xF0);
        let c_ok     = egui::Color32::from_rgb(0x4C, 0xD9, 0x7A);
        let c_err    = egui::Color32::from_rgb(0xFF, 0x5A, 0x5A);
        let c_bar_bg = egui::Color32::from_rgb(0x20, 0x28, 0x20);

        let lbl_close     = self.strings.ia_btn_close().to_owned();
        let py_title      = self.strings.py_title().to_owned();

        // keep_open est retourné depuis la closure via inner_response
        let resp = egui::Window::new(py_title)
            .collapsible(false)
            .resizable(true)
            .default_size([580.0, 400.0])
            .min_size([420.0, 300.0])
            .frame(
                egui::Frame::none()
                    .fill(c_bg)
                    .stroke(egui::Stroke::new(1.5, c_border))
                    .rounding(6.0)
                    .inner_margin(14.0),
            )
            .show(ctx, |ui| {
                let mut close = false;
                let st = shared.lock().unwrap();

                // Barre de statut globale (indéterminée si pas encore terminé)
                let bar_w = ui.available_width() - 4.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, 20.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, c_bar_bg);
                if st.done {
                    let color = if st.error.is_none() { c_ok } else { c_err };
                    ui.painter().rect_filled(rect, 4.0, color);
                    let label = if st.error.is_none() { "✅ Terminé" } else { "❌ Erreur" };
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                        label, egui::FontId::monospace(13.0), egui::Color32::BLACK);
                } else {
                    let t = ctx.input(|i| i.time) as f32;
                    let pulse = ((t * 1.5).sin() * 0.5 + 0.5) as f32;
                    let fill_w = rect.width() * (0.3 + pulse * 0.4);
                    let offset = (rect.width() - fill_w) * pulse;
                    let fr = egui::Rect::from_min_size(
                        rect.min + egui::vec2(offset, 0.0),
                        egui::vec2(fill_w, rect.height()),
                    );
                    ui.painter().rect_filled(fr, 4.0, c_border);
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                        "🐍 setup_venv_lang…", egui::FontId::monospace(12.0), c_text);
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                }

                ui.add_space(6.0);
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(280.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &st.log {
                            let color = if line.contains('⚠') || line.contains("error") || line.contains("Error") {
                                c_err
                            } else if line.contains("✅") {
                                c_ok
                            } else {
                                c_dim
                            };
                            ui.colored_label(color, line);
                        }
                    });

                ui.separator();

                ui.horizontal(|ui| {
                    let close_btn = egui::Button::new(
                        egui::RichText::new(&lbl_close).color(c_text))
                        .fill(egui::Color32::from_rgb(0x1A, 0x28, 0x1A));
                    if ui.add(close_btn).clicked() {
                        close = true;
                    }
                    if let (Some(delay), Some(start)) = (st.auto_close_secs, st.auto_close_start) {
                        let elapsed   = start.elapsed().as_secs() as u32;
                        let remaining = delay.saturating_sub(elapsed);
                        ui.colored_label(c_dim, format!("·  fermeture auto dans {}s", remaining));
                        if remaining == 0 { close = true; }
                        ctx.request_repaint_after(std::time::Duration::from_millis(500));
                    }
                    if let Some(ref err) = st.error {
                        ui.colored_label(c_err, err);
                    }
                });

                !close  // valeur retournée = keep_open
            });

        // Si la fenêtre a été fermée (X) ou close=true
        resp.map(|r| r.inner.unwrap_or(false)).unwrap_or(false)
    }
}

impl eframe::App for BaluchonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Palette ──────────────────────────────────────────────────────────
        let accent   = egui::Color32::from_rgb(0x3D, 0x8B, 0xFF);
        let accent_h = egui::Color32::from_rgb(0x5A, 0xA0, 0xFF);
        let bg_panel = egui::Color32::from_rgb(0x1A, 0x1D, 0x27);
        let bg_item  = egui::Color32::from_rgb(0x22, 0x26, 0x38);
        let bg_sel   = egui::Color32::from_rgb(0x2A, 0x35, 0x5A);
        let fg_dim   = egui::Color32::from_rgb(0x88, 0x90, 0xAA);
        let fg_text  = egui::Color32::from_rgb(0xE0, 0xE4, 0xF0);
        let status_ok = egui::Color32::from_rgb(0x4C, 0xD9, 0x7A);

        let mut visuals = ctx.style().visuals.clone();
        visuals.panel_fill  = bg_panel;
        visuals.window_fill = bg_panel;
        visuals.widgets.noninteractive.bg_fill = bg_item;
        visuals.widgets.inactive.bg_fill   = bg_item;
        visuals.widgets.hovered.bg_fill    = accent_h;
        visuals.widgets.active.bg_fill     = accent;
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(0.0, fg_text);
        visuals.widgets.inactive.fg_stroke = egui::Stroke::new(0.0, fg_text);
        visuals.selection.bg_fill  = bg_sel;
        visuals.selection.stroke   = egui::Stroke::new(1.5, accent);
        visuals.override_text_color = Some(fg_text);
        ctx.set_visuals(visuals);

        // ── Panneau install-assistant (flottant) ──────────────────────────
        if self.install_panel.is_some() {
            let keep = self.draw_install_panel(ctx);
            if !keep { self.install_panel = None; }
        }

        // ── Panneau setup init (flottant) ───────────────────────────────
        if self.setup_panel.is_some() {
            let keep = self.draw_setup_panel(ctx);
            if !keep { self.setup_panel = None; }
        }

        // ── Header ────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(0x12, 0x14, 0x1E)).inner_margin(12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // ── Logo baluchon.png ──────────────────────────────────
                    // Chargement paresseux : on cherche baluchon.png d'abord
                    // à côté de l'exécutable, puis à la racine de la clé USB.
                    if self.logo.is_none() {
                        let candidates: Vec<PathBuf> = {
                            let mut v = vec![];
                            if let Ok(exe) = std::env::current_exe() {
                                if let Some(dir) = exe.parent() {
                                    v.push(dir.join("baluchon.png"));
                                }
                            }
                            if let Some(ref root) = self.usb_root {
                                v.push(root.join("baluchon.png"));
                            }
                            v
                        };
                        for path in candidates {
                            if path.exists() {
                                if let Ok(bytes) = std::fs::read(&path) {
                                    if let Ok(img) = image::load_from_memory(&bytes) {
                                        let rgba = img.to_rgba8();
                                        let (w, h) = rgba.dimensions();
                                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                            [w as usize, h as usize],
                                            rgba.as_raw(),
                                        );
                                        self.logo = Some(ctx.load_texture(
                                            "baluchon_logo",
                                            color_image,
                                            egui::TextureOptions::LINEAR,
                                        ));
                                    }
                                }
                                break;
                            }
                        }
                    }
                    // Affichage de l'icône (32×32 px) ou emoji de repli
                    if let Some(ref tex) = self.logo {
                        let size = egui::vec2(32.0, 32.0);
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                        ui.add_space(8.0);
                    } else {
                        ui.colored_label(accent, "💾");
                        ui.add_space(6.0);
                    }
                    ui.heading(&self.strings.title().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(self.strings.label_lang());
                        ui.add_space(4.0);
                        if ui.selectable_label(self.strings.lang == Lang::En, "EN").clicked() {
                            self.strings.lang = Lang::En;
                        }
                        if ui.selectable_label(self.strings.lang == Lang::Fr, "FR").clicked() {
                            self.strings.lang = Lang::Fr;
                        }
                    });
                });
                ui.add_space(4.0);
                match &self.usb_root {
                    Some(root) => {
                        ui.horizontal(|ui| {
                            ui.colored_label(status_ok, self.strings.label_usb());
                            ui.add_space(4.0);
                            ui.colored_label(fg_dim, root.to_string_lossy().as_ref());
                        });
                    }
                    None => { ui.colored_label(egui::Color32::RED, self.strings.label_no_usb()); }
                }
                if let Some(err) = &self.load_error {
                    ui.colored_label(egui::Color32::RED, format!("❌ {}", err));
                }
            });

        // ── Status bar ────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("statusbar")
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(0x0E, 0x10, 0x18)).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(fg_dim, format!("{}: ", self.strings.label_status()));
                    let color = if self.status.starts_with('❌') {
                        egui::Color32::from_rgb(0xFF, 0x5A, 0x5A)
                    } else { status_ok };
                    ui.colored_label(color, &self.status);
                });
            });

        // ── Panneau droite : actions ──────────────────────────────────────
        egui::SidePanel::right("actions")
            .min_width(220.0)
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(0x16, 0x19, 0x28)).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                let has_sel   = self.selected.is_some() && self.usb_root.is_some();
                let dl_active = self.install_panel.is_some();
                let btn_size  = egui::vec2(200.0, 40.0);

                // Boutons Launch / Install / Remove
                let lbl_launch  = self.strings.btn_launch().to_owned();
                let lbl_install = self.strings.btn_install().to_owned();
                let lbl_remove  = self.strings.btn_remove().to_owned();
                for (label, enabled, action) in [
                    (lbl_launch,  has_sel, 0u8),
                    (lbl_install, has_sel, 1),
                    (lbl_remove,  has_sel, 2),
                ] {
                    let btn = egui::Button::new(label)
                        .min_size(btn_size)
                        .fill(if enabled { accent } else { bg_item });
                    if ui.add_enabled(enabled, btn).clicked() {
                        match action {
                            0 => self.action_launch(),
                            1 => self.action_install(),
                            2 => self.action_remove(),
                            _ => {}
                        }
                    }
                    ui.add_space(8.0);
                }

                // Bouton Télécharger (désactivé si panneau déjà ouvert)
                {
                    let enabled = has_sel && !dl_active;
                    let label   = self.strings.btn_download().to_owned();
                    let btn = egui::Button::new(label)
                        .min_size(btn_size)
                        .fill(if enabled { accent } else { bg_item });
                    if ui.add_enabled(enabled, btn).clicked() {
                        let ctx2 = ctx.clone();
                        self.action_download(&ctx2);
                    }
                    if dl_active {
                        ui.colored_label(fg_dim, "⬇ en cours…");
                    }
                    ui.add_space(8.0);
                }



                // Détails de l'app
                if let Some(app) = self.selected_app() {
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.colored_label(accent, &app.name);
                    if let Some(desc) = &app.description {
                        ui.add_space(4.0);
                        ui.colored_label(fg_dim, desc);
                    }
                    ui.add_space(8.0);
                    let os_label = if cfg!(target_os = "windows") { "Windows" } else { "Linux" };
                    ui.colored_label(fg_dim, format!("OS : {}", os_label));
                    ui.colored_label(fg_dim, format!("Exec : {}", app.exec_path().unwrap_or("—")));
                    if let Some(url) = &app.download_url {
                        ui.add_space(4.0);
                        ui.colored_label(fg_dim, format!("URL : {}", url));
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        ui.add_space(4.0);
                        ui.colored_label(fg_dim, format!("DE : {}", detect_desktop()));
                    }
                }
            });

        // ── Zone centrale : liste des applications ────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg_panel).inner_margin(16.0))
            .show(ctx, |ui| {
                if self.apps.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(fg_dim, self.strings.label_no_apps());
                    });
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let n = self.apps.len();
                    for i in 0..n {
                        let app = &self.apps[i];
                        let is_sel = self.selected == Some(i);
                        let frame = egui::Frame::none()
                            .fill(if is_sel { bg_sel } else { bg_item })
                            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                            .rounding(6.0)
                            .stroke(if is_sel {
                                egui::Stroke::new(1.5, accent)
                            } else {
                                egui::Stroke::new(0.5, egui::Color32::from_rgb(0x30, 0x35, 0x50))
                            });
                        let resp = frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(accent, "📦");
                                ui.add_space(8.0);
                                ui.vertical(|ui| {
                                    ui.colored_label(fg_text, &app.name);
                                    if let Some(desc) = &app.description {
                                        ui.colored_label(fg_dim, desc);
                                    }
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if app.exec_path().is_some() {
                                        ui.colored_label(status_ok, "●");
                                    } else {
                                        ui.colored_label(egui::Color32::RED, "○");
                                    }
                                });
                            });
                        });
                        if resp.response.interact(egui::Sense::click()).clicked() {
                            self.selected = Some(i);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
    }
}

// ── Icône de la fenêtre ────────────────────────────────────────────────────────

fn load_icon() -> egui::IconData {
    let image_bytes = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(image_bytes)
        .expect("Impossible de charger assets/icon.png")
        .to_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

// ── Point d'entrée ────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("Baluchon")
        .with_inner_size([780.0, 520.0])
        .with_min_inner_size([600.0, 400.0])
        .with_icon(load_icon());
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Baluchon",
        options,
        Box::new(|cc| Box::new(BaluchonApp::new(cc)) as Box<dyn eframe::App>),
    )
}
