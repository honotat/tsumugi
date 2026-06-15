use std::collections::HashMap;
use tauri::command;

use crate::history::{EntryDiffPreview, EntryFullDiff, HistoryConfig, HistoryEntryMeta, HistoryFileMeta, HistoryState, UnsavedDiffResult};
use crate::i18n::I18nState;
use crate::recent::{RecentEntry, RecentState};
use crate::state::WindowStates;
use crate::tags::{TagEntry, TagState};


/// ファイルパスを検証・正規化し、機密性の高いシステムディレクトリへのアクセスをブロックする。
pub(crate) fn validate_path(path: &str) -> Result<std::path::PathBuf, String> {
    if path.contains('\0') {
        return Err("Invalid path".to_string());
    }
    let p = std::path::Path::new(path);

    // 正規パスに解決（シンボリックリンク、.. などを解決）
    let canonical = dunce::canonicalize(p).or_else(|_| {
        // 新規ファイルの場合: 親ディレクトリを正規化
        p.parent()
            .ok_or_else(|| "Invalid path".to_string())
            .and_then(|parent| {
                dunce::canonicalize(parent).map_err(|e| format!("Invalid path: {}", e))
            })
            .map(|cp| cp.join(p.file_name().unwrap_or_default()))
    })?;

    // 機密性の高いシステムディレクトリをブロック
    let path_str = canonical.to_string_lossy();
    let mut blocked: Vec<String> = if cfg!(target_os = "windows") {
        // ドライブレター決め打ちを避け環境変数から取得
        let mut v = Vec::new();
        if let Ok(windir) = std::env::var("SystemRoot").or_else(|_| std::env::var("windir")) {
            v.push(windir);
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            v.push(pf);
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            v.push(pf86);
        }
        v
    } else {
        vec![
            "/etc".to_string(),
            "/private/etc".to_string(),
            "/usr".to_string(),
            "/bin".to_string(),
            "/sbin".to_string(),
            "/System".to_string(),
        ]
    };
    // ホームディレクトリ配下の機密パスを追加
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if cfg!(target_os = "windows") {
            blocked.push(format!("{}\\.ssh", home_str));
            blocked.push(format!("{}\\.aws", home_str));
        } else {
            blocked.push(format!("{}/.ssh", home_str));
            blocked.push(format!("{}/.aws", home_str));
            blocked.push(format!("{}/.gnupg", home_str));
        }
    }
    // Windowsは大文字小文字を区別しないため小文字化して比較
    let case_insensitive = cfg!(target_os = "windows");
    let target = if case_insensitive {
        path_str.to_lowercase()
    } else {
        path_str.to_string()
    };
    for prefix in &blocked {
        let needle = if case_insensitive {
            prefix.to_lowercase()
        } else {
            prefix.clone()
        };
        if target.starts_with(&needle) {
            return Err(format!("Access denied: {}", prefix));
        }
    }
    Ok(canonical)
}

/// ドキュメントを読み込み、先頭のUTF-8 BOMを除去する
pub(crate) fn read_document<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<String> {
    let mut s = std::fs::read_to_string(path)?;
    if s.starts_with('\u{feff}') {
        s.remove(0);
    }
    Ok(s)
}

/// 一時ファイルへ書いてから rename するアトミック書き込み
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or_else(|| "Invalid path".to_string())?;
    // 別ボリュームへの rename を避けるため同一ディレクトリに作る
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{}.{}.tmp", file_name, std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("Failed to write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to write {}: {}", path.display(), e)
    })
}

#[command]
pub fn read_file(path: String) -> Result<String, String> {
    let path = validate_path(&path)?.to_string_lossy().to_string();
    read_document(&path).map_err(|e| format!("Failed to read {}: {}", path, e))
}

#[command]
pub fn save_file(path: String, content: String) -> Result<(), String> {
    let path = validate_path(&path)?;
    atomic_write(&path, content.as_bytes())
}

#[command]
pub fn notify_saved(
    path: String,
    window: tauri::Window,
    states: tauri::State<'_, WindowStates>,
    recent: tauri::State<'_, RecentState>,
    history: tauri::State<'_, HistoryState>,
) {
    let current_content = {
        let mut guard = states.lock().unwrap();
        if let Some(state) = guard.get_mut(window.label()) {
            state.saved_path = Some(path.clone());
            state.path_disclosure = true;
            state.saved_content = state.current_content.clone();
            state.dirty = false;
            Some(state.current_content.clone())
        } else {
            None
        }
    };
    // 最近使ったファイルに追加
    let title = std::path::Path::new(&path)
        .file_stem()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".to_string());
    let mut store = recent.lock().unwrap();
    store.add(&path, &title);
    drop(store);
    // 保存時の状態を履歴に記録
    if let Some(content) = current_content {
        let mut hs = history.lock().unwrap();
        hs.record_change(window.label(), &content, Some(&path), true);
    }
}

#[command]
pub fn get_saved_path(window: tauri::Window, states: tauri::State<'_, WindowStates>) -> Option<String> {
    let states = states.lock().unwrap();
    states.get(window.label()).and_then(|s| s.saved_path.clone())
}

#[command]
pub fn sync_content(
    content: String,
    window: tauri::Window,
    states: tauri::State<'_, WindowStates>,
) {
    let mut guard = states.lock().unwrap();
    if let Some(state) = guard.get_mut(window.label()) {
        state.current_content = content.clone();
        state.dirty = content != state.saved_content;
    }
}

#[command]
pub fn record_history(
    window: tauri::Window,
    states: tauri::State<'_, WindowStates>,
    history: tauri::State<'_, HistoryState>,
) {
    let (content, saved_path, is_saved) = {
        let guard = states.lock().unwrap();
        if let Some(state) = guard.get(window.label()) {
            (
                state.current_content.clone(),
                state.saved_path.clone(),
                !state.dirty,
            )
        } else {
            return;
        }
    };
    let mut hs = history.lock().unwrap();
    hs.record_change(window.label(), &content, saved_path.as_deref(), is_saved);
}

#[command]
pub fn set_dirty(dirty: bool, window: tauri::Window, states: tauri::State<'_, WindowStates>) {
    let mut states = states.lock().unwrap();
    if let Some(state) = states.get_mut(window.label()) {
        state.dirty = dirty;
        if !dirty {
            // dirty=falseにリセットする際、現在のコンテンツを基準値として記録
            state.saved_content = state.current_content.clone();
        }
    }
}

#[command]
pub fn get_initial_content(window: tauri::Window, states: tauri::State<'_, WindowStates>) -> (String, String, bool) {
    let states = states.lock().unwrap();
    if let Some(state) = states.get(window.label()) {
        (state.current_content.clone(), state.title.clone(), state.content_explicitly_set)
    } else {
        (String::new(), "Untitled".to_string(), false)
    }
}

#[command]
pub fn rename_file(old_path: String, new_path: String, window: tauri::Window, states: tauri::State<'_, WindowStates>) -> Result<String, String> {
    let old_path = validate_path(&old_path)?.to_string_lossy().to_string();
    let new_path = validate_path(&new_path)?.to_string_lossy().to_string();
    std::fs::rename(&old_path, &new_path)
        .map_err(|e| format!("Failed to rename: {}", e))?;
    let abs_path = dunce::canonicalize(&new_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(&new_path));
    let abs_path_str = crate::normalize_path(&abs_path.to_string_lossy());
    let mut states = states.lock().unwrap();
    if let Some(state) = states.get_mut(window.label()) {
        state.saved_path = Some(abs_path_str.clone());
        state.path_disclosure = true;
        let title = abs_path
            .file_stem()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".to_string());
        state.title = title;
    }
    Ok(abs_path_str)
}

#[command]
pub fn save_binary_file(path: String, data: Vec<u8>) -> Result<(), String> {
    let path = validate_path(&path)?;
    atomic_write(&path, &data)
}

#[command]
pub fn get_translations(i18n: tauri::State<'_, I18nState>) -> HashMap<String, String> {
    let i18n = i18n.lock().unwrap();
    i18n.flat_map()
}

#[command]
pub fn get_custom_locale_path() -> String {
    crate::i18n::custom_locale_path().to_string_lossy().to_string()
}

#[command]
pub fn get_platform() -> String {
    std::env::consts::OS.to_string()
}

#[command]
pub fn execute_menu_action(id: String, app: tauri::AppHandle) {
    crate::menu::execute_action(&app, &id);
}

#[command]
pub fn set_editor_menu_enabled(enabled: bool, app: tauri::AppHandle) {
    crate::menu::set_editor_menu_enabled(&app, enabled);
}

#[command]
pub async fn open_new_window(
    file: Option<String>,
    body: Option<String>,
    close_self: Option<bool>,
    window: tauri::Window,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let close = close_self.unwrap_or(false);
    let app_clone = app.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        crate::open_document_window(&app_clone, file, body, None)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?;

    result.map(|_| ())?;

    if close {
        let _ = window.destroy();
    }

    Ok(())
}

#[command]
pub fn tag_add(path: String, tag: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.add_tag(&path, &tag);
}

#[command]
pub fn tag_remove(path: String, tag: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.remove_tag(&path, &tag);
}

#[command]
pub fn tag_set(path: String, tags: Vec<String>, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.set_tags(&path, tags);
}

#[command]
pub fn tag_get(path: String, state: tauri::State<'_, TagState>) -> Vec<String> {
    let store = state.lock().unwrap();
    store.get_tags(&path)
}

#[command]
pub fn tag_get_all(state: tauri::State<'_, TagState>) -> Vec<TagEntry> {
    let mut store = state.lock().unwrap();
    store.reload();
    store.get_all_entries()
}

#[command]
pub fn tag_delete_entry(path: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.delete_entry(&path);
}

#[command]
pub fn tag_relink(old_path: String, new_path: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.relink(&old_path, &new_path);
}

#[command]
pub fn tag_set_memo(path: String, memo: Option<String>, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.set_memo(&path, memo);
}

#[command]
pub fn restart_app(app: tauri::AppHandle, states: tauri::State<'_, WindowStates>) {
    // プライマリソケットとトークンファイルをクリーンアップ
    let primary_path = crate::ipc::instance_file("tsumugi-primary");
    std::fs::remove_file(&primary_path).ok();
    std::fs::remove_file(primary_path.with_extension("token")).ok();

    // ウィンドウごとのソケット、HTTPポートファイル、トークンファイルをクリーンアップ
    {
        let states = states.lock().unwrap();
        for (_, state) in states.iter() {
            let path = crate::ipc::instance_file(&state.instance_id);
            std::fs::remove_file(&path).ok();
            std::fs::remove_file(path.with_extension("http")).ok();
            std::fs::remove_file(path.with_extension("token")).ok();
        }
    }

    let exe = std::env::current_exe().ok();
    if let Some(exe) = exe {
        let _ = std::process::Command::new(exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn();
    }
    app.exit(0);
}

#[command]
pub fn tag_validate_paths(state: tauri::State<'_, TagState>) -> Vec<(String, bool)> {
    let mut store = state.lock().unwrap();
    store.reload();
    store.validate_paths()
}

#[command]
pub fn tag_get_all_unique_tags(state: tauri::State<'_, TagState>) -> Vec<String> {
    let mut store = state.lock().unwrap();
    store.reload();
    store.get_all_unique_tags()
}

#[command]
pub fn tag_batch_add(paths: Vec<String>, tag: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.batch_add(&paths, &tag);
}

#[command]
pub fn tag_rename_all(old_name: String, new_name: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.rename_all(&old_name, &new_name);
}

#[command]
pub fn tag_remove_all(tag_name: String, state: tauri::State<'_, TagState>) {
    let mut store = state.lock().unwrap();
    store.remove_all(&tag_name);
}

#[command]
pub fn tag_get_counts(state: tauri::State<'_, TagState>) -> Vec<(String, usize)> {
    let mut store = state.lock().unwrap();
    store.reload();
    store.get_counts()
}

// --- 最近使ったファイル ---

#[command]
pub fn recent_get_all(state: tauri::State<'_, RecentState>) -> Vec<RecentEntry> {
    let store = state.lock().unwrap();
    store.get_all()
}

#[command]
pub fn recent_add(path: String, title: String, state: tauri::State<'_, RecentState>) {
    let mut store = state.lock().unwrap();
    store.add(&path, &title);
}

#[command]
pub fn recent_remove(path: String, state: tauri::State<'_, RecentState>) {
    let mut store = state.lock().unwrap();
    store.remove(&path);
}

#[command]
pub fn recent_clear(state: tauri::State<'_, RecentState>) {
    let mut store = state.lock().unwrap();
    store.clear();
}

// --- 変更履歴 ---

#[command]
pub fn history_get_config(state: tauri::State<'_, HistoryState>) -> HistoryConfig {
    let store = state.lock().unwrap();
    store.config().clone()
}

#[command]
pub fn history_set_config(config: HistoryConfig, state: tauri::State<'_, HistoryState>) {
    let mut store = state.lock().unwrap();
    store.set_config(config);
}

#[command]
pub fn history_get_files(history: tauri::State<'_, HistoryState>) -> Vec<HistoryFileMeta> {
    let hs = history.lock().unwrap();
    hs.get_files()
}

#[command]
pub fn history_restore_at(file_hash: String, target_timestamp: u64) -> Result<String, String> {
    crate::history::restore_at(&file_hash, target_timestamp)
}

#[command]
pub fn history_check_unsaved(path: String, history: tauri::State<'_, HistoryState>) -> bool {
    let hs = history.lock().unwrap();
    hs.check_unsaved(&path)
}

#[command]
pub fn history_delete_file(file_hash: String, history: tauri::State<'_, HistoryState>) -> Result<(), String> {
    let mut hs = history.lock().unwrap();
    hs.delete_file(&file_hash)
}

#[command]
pub fn history_delete_files(file_hashes: Vec<String>, history: tauri::State<'_, HistoryState>) -> Result<(), String> {
    let mut hs = history.lock().unwrap();
    for hash in &file_hashes {
        hs.delete_file(hash)?;
    }
    Ok(())
}

#[command]
pub fn history_get_file_hash(path: String) -> String {
    crate::history::path_hash(&path)
}

#[command]
pub fn history_get_entries(file_hash: String) -> Result<Vec<HistoryEntryMeta>, String> {
    crate::history::get_entries(&file_hash)
}

#[command]
pub fn history_get_unsaved_diff(file_hash: String) -> Option<UnsavedDiffResult> {
    crate::history::get_unsaved_diff(&file_hash)
}

#[command]
pub fn history_delete_unsaved(file_hash: String, history: tauri::State<'_, HistoryState>) -> Result<(), String> {
    let mut hs = history.lock().unwrap();
    hs.delete_unsaved_entries(&file_hash)
}

#[command]
pub fn history_get_entry_previews(file_hash: String) -> Result<Vec<EntryDiffPreview>, String> {
    crate::history::get_entry_previews(&file_hash)
}

#[command]
pub fn history_get_entry_diff(file_hash: String, target_timestamp: u64) -> Result<EntryFullDiff, String> {
    crate::history::get_entry_diff(&file_hash, target_timestamp)
}

