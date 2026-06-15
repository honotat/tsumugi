use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::WindowStates;

pub struct FileWatcher {
    _debouncer: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
    watched_path: Mutex<Option<String>>,
}

/// ウィンドウラベル → FileWatcher
pub type FileWatchers = Mutex<HashMap<String, FileWatcher>>;

impl FileWatcher {
    pub fn new() -> Self {
        Self {
            _debouncer: None,
            watched_path: Mutex::new(None),
        }
    }

    pub fn watch(&mut self, app: AppHandle, window_label: String, path: String) {
        // 前のウォッチャーを停止
        self._debouncer = None;

        let watch_path = path.clone();
        let app_handle = app.clone();
        let label = window_label.clone();

        let mut debouncer = match new_debouncer(Duration::from_millis(100), move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, _>| {
            if let Ok(events) = res {
                for event in events {
                    if event.kind == DebouncedEventKind::Any {
                        // ファイルを再読み込みし、状態とフロントエンドを更新
                        if let Ok(content) = crate::commands::read_document(&watch_path) {
                            let mut should_emit = false;
                            {
                                let states = app_handle.state::<WindowStates>();
                                let mut states = states.lock().unwrap();
                                if let Some(state) = states.get_mut(&label) {
                                    // 自分の保存で同じ内容が書かれた場合は無視する
                                    if state.current_content != content {
                                        state.current_content = content.clone();
                                        state.dirty = false;
                                        should_emit = true;
                                    }
                                }
                            }
                            if should_emit {
                                let _ = app_handle.emit_to(
                                    &label as &str,
                                    "content-update",
                                    serde_json::json!({ "body": content }),
                                );
                            }
                        }
                        break;
                    }
                }
            }
        }) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("tsumugi: Failed to create debouncer for {}: {}", path, e);
                return;
            }
        };

        if let Err(e) = debouncer
            .watcher()
            .watch(Path::new(&path), notify::RecursiveMode::NonRecursive)
        {
            eprintln!("tsumugi: Failed to watch file {}: {}", path, e);
            return;
        }

        *self.watched_path.lock().unwrap() = Some(path);
        self._debouncer = Some(debouncer);
    }
}
