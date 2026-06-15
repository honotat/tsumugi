use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// 30日（秒）
const CLEANUP_MAX_AGE_SECS: u64 = 30 * 24 * 3600;

/// 設定
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub snapshot_interval: u32,
    pub include_network_paths: bool,
    pub include_temp_files: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            snapshot_interval: 20,
            include_network_paths: false,
            include_temp_files: false,
        }
    }
}

/// 履歴エントリ（JSONLの各行）
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HistoryEntry {
    #[serde(rename = "snapshot")]
    Snapshot {
        t: u64,
        p: String,
        c: String,
        saved: bool,
    },
    #[serde(rename = "delta")]
    Delta {
        t: u64,
        p: String,
        d: String,
        saved: bool,
    },
}

/// 差分行（未保存デルタ確認モーダル用）
#[derive(Serialize, Deserialize, Clone)]
pub struct DiffLine {
    pub op: String,    // "equal", "delete", "insert"
    pub text: String,
}

/// 未保存デルタの差分結果
#[derive(Serialize, Deserialize, Clone)]
pub struct UnsavedDiffResult {
    pub saved_content: String,
    pub latest_content: String,
    pub diff_lines: Vec<DiffLine>,
    pub unsaved_count: usize,
    pub last_saved_timestamp: u64,
}

/// エントリ差分プレビュー（モーダル一覧用、最大2行）
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryDiffPreview {
    pub timestamp: u64,
    pub preview_lines: Vec<DiffLine>,
    pub total_changes: usize,
}

/// エントリ完全差分（差分詳細モーダル用）
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryFullDiff {
    pub timestamp: u64,
    pub diff_lines: Vec<DiffLine>,
}

/// 履歴エントリのメタ情報（一覧表示用）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HistoryEntryMeta {
    pub entry_type: String,
    pub timestamp: u64,
    pub file_path: String,
    pub saved: bool,
}

/// 履歴ファイルのメタ情報
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HistoryFileMeta {
    pub file_hash: String,
    pub file_path: String,
    pub entry_count: usize,
    pub last_timestamp: u64,
    pub has_unsaved: bool,
}

/// インデックスエントリ（メタ情報のJSON管理用）
#[derive(Debug, Serialize, Deserialize, Clone)]
struct IndexEntry {
    file_path: String,
    entry_count: usize,
    last_timestamp: u64,
    has_unsaved: bool,
}

/// ファイルごとの追跡状態
struct FileTracker {
    last_recorded_content: String,
    delta_count_since_snapshot: u32,
    file_hash: String,
    file_path: Option<String>,
    has_initial_snapshot: bool,
}

/// 履歴ストア
pub struct HistoryStore {
    config: HistoryConfig,
    trackers: HashMap<String, FileTracker>,
    index: HashMap<String, IndexEntry>,
}

pub type HistoryState = Mutex<HistoryStore>;

impl HistoryStore {
    pub fn load() -> Self {
        let config_path = config_path();
        let config = if config_path.exists() {
            std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            HistoryConfig::default()
        };

        // インデックスを読み込み。なければJSONLからビルド
        let idx_path = index_path();
        let mut index = if idx_path.exists() {
            std::fs::read_to_string(&idx_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(build_index_from_jsonl)
        } else {
            let idx = build_index_from_jsonl();
            if !idx.is_empty() {
                save_index_to_disk(&idx);
            }
            idx
        };

        // 旧DefaultHasher時代の履歴ファイルを安定ハッシュ名へ移行
        if migrate_legacy_hashes(&mut index) {
            save_index_to_disk(&index);
        }

        Self {
            config,
            trackers: HashMap::new(),
            index,
        }
    }

    pub fn config(&self) -> &HistoryConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: HistoryConfig) {
        self.config = config;
        self.save_config();
    }

    fn save_config(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            std::fs::write(&path, json).ok();
        }
    }

    fn save_index(&self) {
        save_index_to_disk(&self.index);
    }

    /// 追跡を開始し、初回スナップショットを書き込む
    pub fn start_tracking(
        &mut self,
        label: &str,
        initial_content: &str,
        file_path: Option<&str>,
    ) {
        if !self.config.enabled {
            return;
        }

        // ファイルパスがない場合、include_temp_filesがfalseなら追跡しない
        if file_path.is_none() && !self.config.include_temp_files {
            return;
        }

        // ネットワークパスチェック
        if let Some(fp) = file_path {
            if is_network_path(fp) && !self.config.include_network_paths {
                return;
            }
        }

        let hash = path_hash(file_path.unwrap_or(label));

        // 既存インデックスの has_unsaved をリセット（ディスクから読み込んだ状態）
        if file_path.is_some() {
            if let Some(idx_entry) = self.index.get_mut(&hash) {
                if idx_entry.has_unsaved {
                    idx_entry.has_unsaved = false;
                    self.save_index();
                }
            }
        }

        // 履歴から最新の状態を取得し、初回スナップショットが必要か判定
        let (has_initial, delta_count) = match get_last_state(&hash) {
            Some((last_state, dc)) if last_state == initial_content => (true, dc),
            _ => (false, 0),
        };

        self.trackers.insert(
            label.to_string(),
            FileTracker {
                last_recorded_content: initial_content.to_string(),
                delta_count_since_snapshot: delta_count,
                file_hash: hash,
                file_path: file_path.map(|s| s.to_string()),
                has_initial_snapshot: has_initial,
            },
        );
    }

    /// 追跡を停止
    pub fn stop_tracking(&mut self, label: &str) {
        self.trackers.remove(label);
    }

    /// 変更を記録
    pub fn record_change(
        &mut self,
        label: &str,
        content: &str,
        saved_path: Option<&str>,
        is_saved: bool,
    ) {
        if !self.config.enabled {
            return;
        }

        // 一時ファイルチェック
        if saved_path.is_none() && !self.config.include_temp_files {
            return;
        }

        // ネットワークパスチェック
        if let Some(fp) = saved_path {
            if is_network_path(fp) && !self.config.include_network_paths {
                return;
            }
        }

        let snapshot_interval = self.config.snapshot_interval;

        // トラッカーから必要な値をコピー（借用競合を回避）
        let (file_hash, last_content, delta_count, has_initial_snapshot) =
            match self.trackers.get(label) {
                Some(t) => (
                    t.file_hash.clone(),
                    t.last_recorded_content.clone(),
                    t.delta_count_since_snapshot,
                    t.has_initial_snapshot,
                ),
                None => return,
            };

        let now = now_secs();

        // 初回スナップショットが未記録の場合
        if !has_initial_snapshot {
            if content.is_empty() && last_content.is_empty() {
                return; // 空→空: 何もしない
            }

            let fp_str = saved_path.unwrap_or("").to_string();

            if last_content.is_empty() {
                // 新規ファイル（空→非空）: content でスナップショット、return
                let entry = HistoryEntry::Snapshot {
                    t: now,
                    p: fp_str.clone(),
                    c: content.to_string(),
                    saved: is_saved,
                };
                append_entry(&file_hash, &entry);

                let idx_entry = self
                    .index
                    .entry(file_hash.clone())
                    .or_insert(IndexEntry {
                        file_path: fp_str.clone(),
                        entry_count: 0,
                        last_timestamp: 0,
                        has_unsaved: false,
                    });
                if !fp_str.is_empty() {
                    idx_entry.file_path = fp_str;
                }
                idx_entry.entry_count += 1;
                idx_entry.last_timestamp = now;
                idx_entry.has_unsaved = !is_saved;
                self.save_index();

                if let Some(tracker) = self.trackers.get_mut(label) {
                    tracker.has_initial_snapshot = true;
                    tracker.last_recorded_content = content.to_string();
                }
                return;
            } else {
                // 既存ファイル初回編集: last_content でスナップショット → return せずデルタ処理へ
                let entry = HistoryEntry::Snapshot {
                    t: now,
                    p: fp_str.clone(),
                    c: last_content.clone(),
                    saved: true,
                };
                append_entry(&file_hash, &entry);

                let idx_entry = self
                    .index
                    .entry(file_hash.clone())
                    .or_insert(IndexEntry {
                        file_path: fp_str.clone(),
                        entry_count: 0,
                        last_timestamp: 0,
                        has_unsaved: false,
                    });
                if !fp_str.is_empty() {
                    idx_entry.file_path = fp_str;
                }
                idx_entry.entry_count += 1;
                idx_entry.last_timestamp = now;
                self.save_index();

                if let Some(tracker) = self.trackers.get_mut(label) {
                    tracker.has_initial_snapshot = true;
                }
                // return しない → 以下のデルタ計算に進む
            }
        }

        // 変更なしチェック
        if content == last_content {
            if !is_saved {
                return;
            }
            // 保存イベント: スナップショットを記録
            let path_str = saved_path.unwrap_or("").to_string();
            let entry = HistoryEntry::Snapshot {
                t: now,
                p: path_str.clone(),
                c: content.to_string(),
                saved: true,
            };
            append_entry(&file_hash, &entry);

            let idx_entry = self
                .index
                .entry(file_hash.clone())
                .or_insert(IndexEntry {
                    file_path: path_str.clone(),
                    entry_count: 0,
                    last_timestamp: 0,
                    has_unsaved: false,
                });
            if !path_str.is_empty() {
                idx_entry.file_path = path_str;
            }
            idx_entry.entry_count += 1;
            idx_entry.last_timestamp = now;
            idx_entry.has_unsaved = false;
            self.save_index();

            if let Some(tracker) = self.trackers.get_mut(label) {
                tracker.delta_count_since_snapshot = 0;
            }
            return;
        }

        // saved_pathが変わった場合（名前を付けて保存した場合等）、ハッシュを更新
        if let Some(fp) = saved_path {
            let new_hash = path_hash(fp);
            if new_hash != file_hash {
                // 新しいファイルとして初回スナップショットを書き込む
                let entry = HistoryEntry::Snapshot {
                    t: now,
                    p: fp.to_string(),
                    c: content.to_string(),
                    saved: is_saved,
                };
                append_entry(&new_hash, &entry);

                // インデックスを更新
                let idx_entry = self
                    .index
                    .entry(new_hash.clone())
                    .or_insert(IndexEntry {
                        file_path: fp.to_string(),
                        entry_count: 0,
                        last_timestamp: 0,
                        has_unsaved: false,
                    });
                idx_entry.file_path = fp.to_string();
                idx_entry.entry_count += 1;
                idx_entry.last_timestamp = now;
                idx_entry.has_unsaved = !is_saved;
                self.save_index();

                if let Some(tracker) = self.trackers.get_mut(label) {
                    tracker.file_hash = new_hash;
                    tracker.file_path = Some(fp.to_string());
                    tracker.last_recorded_content = content.to_string();
                    tracker.delta_count_since_snapshot = 0;
                }
                return;
            }
        }

        // 差分を計算
        let path_str = saved_path.unwrap_or("").to_string();
        let dmp = diff_match_patch_rs::DiffMatchPatch::new();
        match dmp.diff_main::<diff_match_patch_rs::Compat>(&last_content, content) {
            Ok(diffs) => {
                match dmp.diff_to_delta(&diffs) {
                    Ok(delta) => {
                        let entry = HistoryEntry::Delta {
                            t: now,
                            p: path_str.clone(),
                            d: delta,
                            saved: is_saved,
                        };
                        append_entry(&file_hash, &entry);
                        let new_delta_count = delta_count + 1;

                        // スナップショット間隔に達した場合、または保存時
                        let wrote_snapshot = if new_delta_count >= snapshot_interval || is_saved {
                            let snapshot = HistoryEntry::Snapshot {
                                t: now,
                                p: path_str.clone(),
                                c: content.to_string(),
                                saved: is_saved,
                            };
                            append_entry(&file_hash, &snapshot);
                            true
                        } else {
                            false
                        };

                        // インデックスを更新（delta + スナップショット分をまとめて）
                        let added = if wrote_snapshot { 2 } else { 1 };
                        let idx_entry = self
                            .index
                            .entry(file_hash.clone())
                            .or_insert(IndexEntry {
                                file_path: path_str.clone(),
                                entry_count: 0,
                                last_timestamp: 0,
                                has_unsaved: false,
                            });
                        if !path_str.is_empty() {
                            idx_entry.file_path = path_str;
                        }
                        idx_entry.entry_count += added;
                        idx_entry.last_timestamp = now;
                        idx_entry.has_unsaved = !is_saved;
                        self.save_index();

                        // トラッカーを更新
                        if let Some(tracker) = self.trackers.get_mut(label) {
                            tracker.delta_count_since_snapshot = if wrote_snapshot {
                                0
                            } else {
                                new_delta_count
                            };
                        }
                    }
                    Err(e) => {
                        eprintln!("tsumugi: history delta error: {:?}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("tsumugi: history diff error: {:?}", e);
            }
        }

        if let Some(tracker) = self.trackers.get_mut(label) {
            tracker.last_recorded_content = content.to_string();
        }
    }

    /// 対象ファイルの未保存チェック（インデックスから取得）
    pub fn check_unsaved(&self, file_path: &str) -> bool {
        let hash = path_hash(file_path);
        self.index.get(&hash).map_or(false, |e| e.has_unsaved)
    }

    /// 履歴ファイル一覧をインデックスから取得
    pub fn get_files(&self) -> Vec<HistoryFileMeta> {
        let mut result: Vec<HistoryFileMeta> = self
            .index
            .iter()
            .map(|(hash, entry)| HistoryFileMeta {
                file_hash: hash.clone(),
                file_path: entry.file_path.clone(),
                entry_count: entry.entry_count,
                last_timestamp: entry.last_timestamp,
                has_unsaved: entry.has_unsaved,
            })
            .collect();
        // 最終タイムスタンプの降順でソート
        result.sort_by(|a, b| b.last_timestamp.cmp(&a.last_timestamp));
        result
    }

    /// 履歴ファイルを削除
    pub fn delete_file(&mut self, file_hash: &str) -> Result<(), String> {
        let path = history_file_path(file_hash);
        std::fs::remove_file(&path).map_err(|e| format!("Failed to delete: {}", e))?;
        self.index.remove(file_hash);
        self.save_index();
        Ok(())
    }

    /// 未保存エントリを削除（最後のsaved=true以降のsaved=falseを除外）
    pub fn delete_unsaved_entries(&mut self, file_hash: &str) -> Result<(), String> {
        let path = history_file_path(file_hash);
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("Failed to read history: {}", e))?;

        let lines: Vec<&str> = content.lines().collect();

        // 最後のsaved=trueエントリのインデックスを特定
        let mut last_saved_idx: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                let saved = match &entry {
                    HistoryEntry::Snapshot { saved, .. } => *saved,
                    HistoryEntry::Delta { saved, .. } => *saved,
                };
                if saved {
                    last_saved_idx = Some(i);
                }
            }
        }

        // saved=trueが一つもない場合は何もしない
        let last_saved = match last_saved_idx {
            Some(idx) => idx,
            None => return Ok(()),
        };

        // last_saved以降のsaved=falseエントリを除外
        let mut keep_lines: Vec<&str> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if i <= last_saved {
                keep_lines.push(line);
            } else {
                // last_saved以降: saved=trueのみ保持
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                    let saved = match &entry {
                        HistoryEntry::Snapshot { saved, .. } => *saved,
                        HistoryEntry::Delta { saved, .. } => *saved,
                    };
                    if saved {
                        keep_lines.push(line);
                    }
                }
            }
        }

        // 再書き込み
        let new_content = keep_lines.join("\n") + "\n";
        std::fs::write(&path, new_content)
            .map_err(|e| format!("Failed to write history: {}", e))?;

        // インデックスを更新
        if let Some(idx_entry) = self.index.get_mut(file_hash) {
            idx_entry.entry_count = keep_lines.len();
            idx_entry.has_unsaved = false;
        }
        self.save_index();

        Ok(())
    }

    /// 古いエントリをクリーンアップ
    pub fn cleanup_old_entries(&mut self) {
        let dir = storage_dir();
        if !dir.exists() {
            return;
        }

        let cutoff = now_secs().saturating_sub(CLEANUP_MAX_AGE_SECS);

        if let Ok(dir_entries) = std::fs::read_dir(&dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(true, |e| e != "jsonl") {
                    continue;
                }

                let file_hash = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                if let Ok(content) = std::fs::read_to_string(&path) {
                    let lines: Vec<&str> = content.lines().collect();
                    // cutoff以降と起点となる直近スナップショット1件を残す
                    let mut base_snapshot: Option<&str> = None;
                    let mut recent_lines = Vec::new();

                    for line in &lines {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                            let t = match &entry {
                                HistoryEntry::Snapshot { t, .. } => *t,
                                HistoryEntry::Delta { t, .. } => *t,
                            };
                            if t >= cutoff {
                                recent_lines.push(*line);
                            } else if matches!(entry, HistoryEntry::Snapshot { .. }) {
                                // 最後の古いスナップショットを起点として残す
                                base_snapshot = Some(*line);
                            }
                        }
                    }

                    let mut keep_lines = Vec::new();
                    if let Some(base) = base_snapshot {
                        keep_lines.push(base);
                    }
                    keep_lines.extend(recent_lines);

                    if keep_lines.len() < lines.len() {
                        if keep_lines.is_empty() {
                            std::fs::remove_file(&path).ok();
                            self.index.remove(&file_hash);
                        } else {
                            let new_content = keep_lines.join("\n") + "\n";
                            std::fs::write(&path, new_content).ok();
                            // インデックスのentry_countを更新
                            if let Some(idx_entry) = self.index.get_mut(&file_hash) {
                                idx_entry.entry_count = keep_lines.len();
                            }
                        }
                    }
                }
            }
        }

        self.save_index();
    }
}

// --- 独立関数 ---

/// 指定ファイルハッシュのエントリ一覧を取得する
pub fn get_entries(file_hash: &str) -> Result<Vec<HistoryEntryMeta>, String> {
    let path = history_file_path(file_hash);
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read history: {}", e))?;

    let mut result = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
            let meta = match &entry {
                HistoryEntry::Snapshot { t, p, saved, .. } => HistoryEntryMeta {
                    entry_type: "snapshot".to_string(),
                    timestamp: *t,
                    file_path: p.clone(),
                    saved: *saved,
                },
                HistoryEntry::Delta { t, p, saved, .. } => HistoryEntryMeta {
                    entry_type: "delta".to_string(),
                    timestamp: *t,
                    file_path: p.clone(),
                    saved: *saved,
                },
            };
            result.push(meta);
        }
    }

    // 新しい順にソート
    result.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(result)
}

/// 指定タイムスタンプの時点のコンテンツを復元する
pub fn restore_at(file_hash: &str, target_timestamp: u64) -> Result<String, String> {
    let path = history_file_path(file_hash);
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read history: {}", e))?;

    let mut entries: Vec<HistoryEntry> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(entry) => entries.push(entry),
            Err(_) => continue, // 壊れた行はスキップ
        }
    }

    if entries.is_empty() {
        return Err("No history entries".to_string());
    }

    // target_timestamp == 0 の場合、最新の状態を復元
    let target = if target_timestamp == 0 {
        u64::MAX
    } else {
        target_timestamp
    };

    // ターゲット以前の最後のスナップショットを見つける
    let mut last_snapshot_idx = None;
    for (i, entry) in entries.iter().enumerate() {
        let t = match entry {
            HistoryEntry::Snapshot { t, .. } => *t,
            HistoryEntry::Delta { t, .. } => *t,
        };
        if t > target {
            break;
        }
        if matches!(entry, HistoryEntry::Snapshot { .. }) {
            last_snapshot_idx = Some(i);
        }
    }

    let snapshot_idx = last_snapshot_idx.ok_or("No snapshot found")?;
    let mut restored = match &entries[snapshot_idx] {
        HistoryEntry::Snapshot { c, .. } => c.clone(),
        _ => unreachable!(),
    };

    // スナップショット以降のデルタを適用
    let dmp = diff_match_patch_rs::DiffMatchPatch::new();
    for entry in &entries[snapshot_idx + 1..] {
        let t = match entry {
            HistoryEntry::Delta { t, .. } => *t,
            HistoryEntry::Snapshot { t, .. } => *t,
        };
        if t > target {
            break;
        }
        match entry {
            HistoryEntry::Delta { d, .. } => {
                match dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&restored, d) {
                    Ok(diffs) => {
                        restored = diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                            .into_iter()
                            .collect();
                    }
                    Err(e) => {
                        eprintln!("tsumugi: history restore delta error: {:?}", e);
                        continue;
                    }
                }
            }
            HistoryEntry::Snapshot { c, .. } => {
                restored = c.clone();
            }
        }
    }

    Ok(restored)
}

/// 履歴ファイルから最新の状態と最後のスナップショットからのデルタ数を取得
fn get_last_state(file_hash: &str) -> Option<(String, u32)> {
    let path = history_file_path(file_hash);
    let raw = std::fs::read_to_string(&path).ok()?;

    let mut current: Option<String> = None;
    let mut delta_count: u32 = 0;
    let dmp = diff_match_patch_rs::DiffMatchPatch::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
            match entry {
                HistoryEntry::Snapshot { c, .. } => {
                    current = Some(c);
                    delta_count = 0;
                }
                HistoryEntry::Delta { d, .. } => {
                    if let Some(ref cur) = current {
                        if let Ok(diffs) =
                            dmp.diff_from_delta::<diff_match_patch_rs::Compat>(cur, &d)
                        {
                            current = Some(
                                diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                                    .into_iter()
                                    .collect(),
                            );
                            delta_count += 1;
                        }
                    }
                }
            }
        }
    }

    current.map(|c| (c, delta_count))
}

/// 未保存デルタの差分を取得する（JSONLファイルを直接解析）
pub fn get_unsaved_diff(file_hash: &str) -> Option<UnsavedDiffResult> {
    let path = history_file_path(file_hash);
    let raw = std::fs::read_to_string(&path).ok()?;

    let mut entries: Vec<HistoryEntry> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str(line) {
            entries.push(entry);
        }
    }

    if entries.is_empty() {
        return None;
    }

    // 最後のsaved=trueエントリのインデックスを特定
    let mut last_saved_idx: Option<usize> = None;
    for (i, entry) in entries.iter().enumerate() {
        let saved = match entry {
            HistoryEntry::Snapshot { saved, .. } => *saved,
            HistoryEntry::Delta { saved, .. } => *saved,
        };
        if saved {
            last_saved_idx = Some(i);
        }
    }

    let last_saved = last_saved_idx?;

    // last_saved以降にsaved=falseエントリがあるか確認
    let unsaved_count = entries[last_saved + 1..]
        .iter()
        .filter(|e| {
            let saved = match e {
                HistoryEntry::Snapshot { saved, .. } => *saved,
                HistoryEntry::Delta { saved, .. } => *saved,
            };
            !saved
        })
        .count();

    if unsaved_count == 0 {
        return None;
    }

    // last_saved_timestamp
    let last_saved_timestamp = match &entries[last_saved] {
        HistoryEntry::Snapshot { t, .. } => *t,
        HistoryEntry::Delta { t, .. } => *t,
    };

    // saved_content: last_savedまで復元
    let dmp = diff_match_patch_rs::DiffMatchPatch::new();
    let mut saved_content = String::new();
    {
        // last_saved以前の最後のスナップショットを探す
        let mut snap_idx = None;
        for i in (0..=last_saved).rev() {
            if matches!(entries[i], HistoryEntry::Snapshot { .. }) {
                snap_idx = Some(i);
                break;
            }
        }
        if let Some(si) = snap_idx {
            saved_content = match &entries[si] {
                HistoryEntry::Snapshot { c, .. } => c.clone(),
                _ => unreachable!(),
            };
            for entry in &entries[si + 1..=last_saved] {
                match entry {
                    HistoryEntry::Delta { d, .. } => {
                        if let Ok(diffs) =
                            dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&saved_content, d)
                        {
                            saved_content =
                                diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                                    .into_iter()
                                    .collect();
                        }
                    }
                    HistoryEntry::Snapshot { c, .. } => {
                        saved_content = c.clone();
                    }
                }
            }
        }
    }

    // latest_content: 全エントリ適用後の最新状態
    let mut latest_content = saved_content.clone();
    for entry in &entries[last_saved + 1..] {
        match entry {
            HistoryEntry::Delta { d, .. } => {
                if let Ok(diffs) =
                    dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&latest_content, d)
                {
                    latest_content = diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                        .into_iter()
                        .collect();
                }
            }
            HistoryEntry::Snapshot { c, .. } => {
                latest_content = c.clone();
            }
        }
    }

    // 行単位diffを生成
    let saved_lines: Vec<&str> = saved_content.split('\n').collect();
    let latest_lines: Vec<&str> = latest_content.split('\n').collect();

    let mut diff_lines = Vec::new();

    // diff_mainで文字レベルの差分を取得し、行単位に再構成
    if let Ok(diffs) =
        dmp.diff_main::<diff_match_patch_rs::Compat>(&saved_content, &latest_content)
    {
        // 行単位に再構成: 削除行と挿入行をまとめて出力
        // シンプルなアプローチ: 行単位で比較
        let _ = diffs; // 文字レベルdiffは使わず、行単位で直接比較

        // LCS（最長共通部分列）ベースの行単位diff
        let lcs = line_lcs(&saved_lines, &latest_lines);
        let mut si = 0;
        let mut li = 0;
        let mut ci = 0;

        while si < saved_lines.len() || li < latest_lines.len() {
            if ci < lcs.len() && si < saved_lines.len() && li < latest_lines.len()
                && saved_lines[si] == lcs[ci] && latest_lines[li] == lcs[ci]
            {
                diff_lines.push(DiffLine {
                    op: "equal".to_string(),
                    text: saved_lines[si].to_string(),
                });
                si += 1;
                li += 1;
                ci += 1;
            } else {
                // 削除行を出力
                while si < saved_lines.len()
                    && (ci >= lcs.len() || saved_lines[si] != lcs[ci])
                {
                    diff_lines.push(DiffLine {
                        op: "delete".to_string(),
                        text: saved_lines[si].to_string(),
                    });
                    si += 1;
                }
                // 挿入行を出力
                while li < latest_lines.len()
                    && (ci >= lcs.len() || latest_lines[li] != lcs[ci])
                {
                    diff_lines.push(DiffLine {
                        op: "insert".to_string(),
                        text: latest_lines[li].to_string(),
                    });
                    li += 1;
                }
            }
        }
    } else {
        // diff_mainが失敗した場合のフォールバック
        for line in &saved_lines {
            diff_lines.push(DiffLine {
                op: "delete".to_string(),
                text: line.to_string(),
            });
        }
        for line in &latest_lines {
            diff_lines.push(DiffLine {
                op: "insert".to_string(),
                text: line.to_string(),
            });
        }
    }

    Some(UnsavedDiffResult {
        saved_content,
        latest_content,
        diff_lines,
        unsaved_count,
        last_saved_timestamp,
    })
}

/// 先頭・末尾一致スキップで変更範囲を特定し、先頭2行のdelete/insertを返す（O(lines)）
fn quick_diff_preview(before: &str, after: &str) -> (Vec<DiffLine>, usize) {
    let before_lines: Vec<&str> = before.split('\n').collect();
    let after_lines: Vec<&str> = after.split('\n').collect();
    let blen = before_lines.len();
    let alen = after_lines.len();

    // 先頭から一致する行数
    let mut prefix = 0;
    while prefix < blen && prefix < alen && before_lines[prefix] == after_lines[prefix] {
        prefix += 1;
    }

    // 末尾から一致する行数（先頭一致分を超えない）
    let mut suffix = 0;
    while suffix < (blen - prefix) && suffix < (alen - prefix)
        && before_lines[blen - 1 - suffix] == after_lines[alen - 1 - suffix]
    {
        suffix += 1;
    }

    let del_count = blen - prefix - suffix;
    let ins_count = alen - prefix - suffix;
    let total_changes = del_count + ins_count;

    let mut preview = Vec::new();
    // 最初の削除行（1行まで）
    if del_count > 0 {
        preview.push(DiffLine {
            op: "delete".to_string(),
            text: before_lines[prefix].to_string(),
        });
    }
    // 最初の挿入行（1行まで）
    if ins_count > 0 && preview.len() < 2 {
        preview.push(DiffLine {
            op: "insert".to_string(),
            text: after_lines[prefix].to_string(),
        });
    }

    (preview, total_changes)
}

/// 全エントリの差分プレビューを一括取得する（saved=trueはスキップ）
pub fn get_entry_previews(file_hash: &str) -> Result<Vec<EntryDiffPreview>, String> {
    let path = history_file_path(file_hash);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read history: {}", e))?;

    let dmp = diff_match_patch_rs::DiffMatchPatch::new();
    let mut current_content = String::new();
    let mut previews = Vec::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: HistoryEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        match &entry {
            HistoryEntry::Snapshot { t, saved, c, .. } => {
                if !*saved {
                    let (preview_lines, total_changes) = quick_diff_preview(&current_content, c);
                    if total_changes > 0 {
                        previews.push(EntryDiffPreview {
                            timestamp: *t,
                            preview_lines,
                            total_changes,
                        });
                    }
                }
                current_content = c.clone();
            }
            HistoryEntry::Delta { t, d, saved, .. } => {
                let before = current_content.clone();
                match dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&current_content, d) {
                    Ok(diffs) => {
                        current_content = diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                            .into_iter()
                            .collect();
                    }
                    Err(_) => continue,
                }
                if !*saved {
                    let (preview_lines, total_changes) = quick_diff_preview(&before, &current_content);
                    if total_changes > 0 {
                        previews.push(EntryDiffPreview {
                            timestamp: *t,
                            preview_lines,
                            total_changes,
                        });
                    }
                }
            }
        }
    }

    Ok(previews)
}

/// 指定タイムスタンプのエントリの完全差分を取得する（LCSベース）
pub fn get_entry_diff(file_hash: &str, target_timestamp: u64) -> Result<EntryFullDiff, String> {
    let path = history_file_path(file_hash);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read history: {}", e))?;

    let dmp = diff_match_patch_rs::DiffMatchPatch::new();
    let mut current_content = String::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: HistoryEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let t = match &entry {
            HistoryEntry::Snapshot { t, .. } => *t,
            HistoryEntry::Delta { t, .. } => *t,
        };

        if t == target_timestamp {
            let before = current_content.clone();
            let after = match &entry {
                HistoryEntry::Snapshot { c, .. } => c.clone(),
                HistoryEntry::Delta { d, .. } => {
                    match dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&current_content, d) {
                        Ok(diffs) => {
                            diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                                .into_iter()
                                .collect()
                        }
                        Err(e) => return Err(format!("Delta error: {:?}", e)),
                    }
                }
            };

            // LCSベースの行単位diff
            let before_lines: Vec<&str> = before.split('\n').collect();
            let after_lines: Vec<&str> = after.split('\n').collect();
            let lcs = line_lcs(&before_lines, &after_lines);

            let mut diff_lines = Vec::new();
            let mut si = 0;
            let mut li = 0;
            let mut ci = 0;

            while si < before_lines.len() || li < after_lines.len() {
                if ci < lcs.len()
                    && si < before_lines.len()
                    && li < after_lines.len()
                    && before_lines[si] == lcs[ci]
                    && after_lines[li] == lcs[ci]
                {
                    diff_lines.push(DiffLine {
                        op: "equal".to_string(),
                        text: before_lines[si].to_string(),
                    });
                    si += 1;
                    li += 1;
                    ci += 1;
                } else {
                    while si < before_lines.len()
                        && (ci >= lcs.len() || before_lines[si] != lcs[ci])
                    {
                        diff_lines.push(DiffLine {
                            op: "delete".to_string(),
                            text: before_lines[si].to_string(),
                        });
                        si += 1;
                    }
                    while li < after_lines.len()
                        && (ci >= lcs.len() || after_lines[li] != lcs[ci])
                    {
                        diff_lines.push(DiffLine {
                            op: "insert".to_string(),
                            text: after_lines[li].to_string(),
                        });
                        li += 1;
                    }
                }
            }

            return Ok(EntryFullDiff {
                timestamp: target_timestamp,
                diff_lines,
            });
        }

        // 状態を更新
        match &entry {
            HistoryEntry::Snapshot { c, .. } => {
                current_content = c.clone();
            }
            HistoryEntry::Delta { d, .. } => {
                match dmp.diff_from_delta::<diff_match_patch_rs::Compat>(&current_content, d) {
                    Ok(diffs) => {
                        current_content = diff_match_patch_rs::DiffMatchPatch::diff_text_new(&diffs)
                            .into_iter()
                            .collect();
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    Err("Entry not found".to_string())
}

/// 行単位LCS（最長共通部分列）を計算
fn line_lcs<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<&'a str> {
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0u32; n + 1]; m + 1];

    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            result.push(a[i - 1]);
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    result.reverse();
    result
}

/// パスのハッシュを計算（lib.rs:file_to_id と同じ方式）
pub fn path_hash(path: &str) -> String {
    let canonical =
        dunce::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    let path_str = canonical.to_string_lossy();
    format!("{:016x}", crate::stable_hash::fnv1a64(path_str.as_bytes()))
}

// --- ヘルパー ---

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn storage_dir() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    } else if cfg!(target_os = "windows") {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    };
    base.join("tsumugi").join("history")
}

fn config_path() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    } else if cfg!(target_os = "windows") {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    };
    base.join("tsumugi").join("history_config.json")
}

fn history_file_path(file_hash: &str) -> PathBuf {
    storage_dir().join(format!("{}.jsonl", file_hash))
}

fn index_path() -> PathBuf {
    storage_dir().join("index.json")
}

fn is_network_path(path: &str) -> bool {
    path.starts_with("\\\\")
        || path.starts_with("//")
        || path.starts_with("\\\\?\\UNC\\")
        || path.starts_with("/Volumes/")
}

fn append_entry(file_hash: &str, entry: &HistoryEntry) {
    let path = history_file_path(file_hash);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string(entry) {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            writeln!(file, "{}", json).ok();
        }
    }
}

fn save_index_to_disk(index: &HashMap<String, IndexEntry>) {
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string(index) {
        std::fs::write(&path, json).ok();
    }
}

/// 旧ハッシュ名の履歴ファイルを現行の安定ハッシュ名へリネームする
/// パスを持つ実ファイルのエントリのみ対象とし、衝突時はスキップする
fn migrate_legacy_hashes(index: &mut HashMap<String, IndexEntry>) -> bool {
    let mut changed = false;
    let old_ids: Vec<String> = index.keys().cloned().collect();
    for old_id in old_ids {
        let file_path = match index.get(&old_id) {
            Some(e) if !e.file_path.is_empty() => e.file_path.clone(),
            _ => continue,
        };
        let new_id = path_hash(&file_path);
        if new_id == old_id {
            continue;
        }
        let old_file = history_file_path(&old_id);
        let new_file = history_file_path(&new_id);
        if !old_file.exists() || new_file.exists() {
            continue;
        }
        if std::fs::rename(&old_file, &new_file).is_ok() {
            if let Some(entry) = index.remove(&old_id) {
                index.insert(new_id, entry);
            }
            changed = true;
        }
    }
    changed
}

/// JSONLファイルからインデックスをビルド（初回マイグレーション用）
fn build_index_from_jsonl() -> HashMap<String, IndexEntry> {
    let dir = storage_dir();
    if !dir.exists() {
        return HashMap::new();
    }

    let mut index = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(true, |e| e != "jsonl") {
                continue;
            }

            let file_hash = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut file_path = String::new();
                let mut entry_count = 0;
                let mut last_timestamp = 0u64;
                let mut has_unsaved = false;

                for line in content.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(he) = serde_json::from_str::<HistoryEntry>(line) {
                        entry_count += 1;
                        match &he {
                            HistoryEntry::Snapshot { t, p, saved, .. } => {
                                if file_path.is_empty() {
                                    file_path = p.clone();
                                }
                                if *t > last_timestamp {
                                    last_timestamp = *t;
                                }
                                has_unsaved = !saved;
                            }
                            HistoryEntry::Delta { t, p, saved, .. } => {
                                if file_path.is_empty() {
                                    file_path = p.clone();
                                }
                                if *t > last_timestamp {
                                    last_timestamp = *t;
                                }
                                has_unsaved = !saved;
                            }
                        }
                    }
                }

                if entry_count > 0 {
                    index.insert(
                        file_hash,
                        IndexEntry {
                            file_path,
                            entry_count,
                            last_timestamp,
                            has_unsaved,
                        },
                    );
                }
            }
        }
    }

    index
}
