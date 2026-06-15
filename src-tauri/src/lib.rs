mod cli;
mod commands;
mod history;
mod http_api;
mod i18n;
mod ipc;
mod menu;
pub(crate) mod recent;
mod state;
mod stable_hash;
mod tags;
mod pdf;
mod watcher;

use std::collections::HashMap;
use std::sync::Mutex;

use clap::Parser;
#[allow(unused_imports)]
use tauri::{Emitter as _, Manager as _};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult};
use history::{HistoryState, HistoryStore};
use recent::{RecentState, RecentStore};
use state::{LastFocusedDoc, WindowState, WindowStates};
use tags::{TagState, TagStore};
use watcher::{FileWatcher, FileWatchers};

/// 正規化されたパスからWindowsの拡張パスプレフィックス (`\\?\`) を除去する。
pub(crate) fn normalize_path(path: &str) -> String {
    dunce::simplified(std::path::Path::new(path))
        .to_string_lossy()
        .into_owned()
}

/// 現在のプロセスで新しいドキュメントウィンドウを作成する。
/// 新しいウィンドウの instance_id を返す。
/// 同じファイルが既に開かれている場合はそのウィンドウをフォーカスし、既存の instance_id を返す。
pub(crate) fn open_document_window(
    app: &tauri::AppHandle,
    file: Option<String>,
    body: Option<String>,
    title: Option<String>,
) -> Result<String, String> {
    // ファイル指定の場合、同じファイルが既に開かれているか確認
    if let Some(ref f) = file {
        let abs_path = dunce::canonicalize(f)
            .unwrap_or_else(|_| std::path::PathBuf::from(f));
        let abs_str = normalize_path(&abs_path.to_string_lossy());

        let existing = {
            let states = app.state::<WindowStates>();
            let guard = states.lock().unwrap();
            guard.iter().find_map(|(label, state)| {
                if state.saved_path.as_deref() == Some(&abs_str) {
                    Some((label.clone(), state.instance_id.clone()))
                } else {
                    None
                }
            })
        };

        if let Some((existing_label, existing_id)) = existing {
            // 既存ウィンドウをフォーカスして手前に表示
            if let Some(window) = app.get_webview_window(&existing_label) {
                force_foreground(&window);
            }
            eprintln!("tsumugi: file already open in window {} (instance: {})", existing_label, existing_id);
            return Ok(existing_id);
        }
    }

    // 一意なラベルを生成
    let label = format!("doc-{:04x}", rand_u16());

    // インスタンスIDを生成
    let instance_id = if let Some(ref f) = file {
        file_to_id(f)
    } else if body.is_some() {
        format!("body-{:04x}", rand_u16())
    } else {
        format!("gui-{:04x}", rand_u16())
    };

    // ファイルパスのバリデーション
    if let Some(ref f) = file {
        commands::validate_path(f)?;
    }

    // コンテンツを解決
    let (content, doc_title, file_path) = if let Some(ref f) = file {
        let abs_path = dunce::canonicalize(f)
            .unwrap_or_else(|_| std::path::PathBuf::from(f));
        let abs_str = normalize_path(&abs_path.to_string_lossy());
        match commands::read_document(&abs_path) {
            Ok(c) => {
                let t = abs_path
                    .file_stem()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Untitled".to_string());
                (c, t, Some(abs_str))
            }
            Err(e) => return Err(format!("Failed to read file: {}", e)),
        }
    } else if let Some(ref b) = body {
        (b.clone(), title.clone().unwrap_or_else(|| "Untitled".to_string()), None)
    } else {
        (String::new(), title.clone().unwrap_or_else(|| "Untitled".to_string()), None)
    };

    // ウィンドウ状態を作成
    let mut ws = WindowState::new(instance_id.clone(), doc_title.clone(), content.clone());
    ws.content_explicitly_set = file.is_some() || body.is_some();
    if let Some(ref fp) = file_path {
        ws.saved_path = Some(fp.clone());
        ws.path_disclosure = true;
    }

    // WindowStates に挿入
    {
        let states = app.state::<WindowStates>();
        states.lock().unwrap().insert(label.clone(), ws);
    }

    // ウィンドウを作成
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        &label,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title(format!("{} — tsumugi", doc_title))
    .inner_size(900.0, 700.0)
    .min_inner_size(400.0, 300.0);

    #[cfg(target_os = "windows")]
    let builder = builder.decorations(false);

    let window = builder.build().map_err(|e| e.to_string())?;

    // 新しいウィンドウをフォーカスして手前に表示
    force_foreground(&window);

    // ウィンドウごとの IPC リスナーを開始
    ipc::start_listener(instance_id.clone(), label.clone(), app.clone());

    // HTTPポートファイルを書き込み（共有サーバーと同じポート）
    {
        let http_info = app.state::<http_api::HttpServerInfo>();
        let port_path = ipc::instance_file(&instance_id).with_extension("http");
        write_port_file(&port_path, &format!("{}:{}", http_info.port, http_info.token));
    }

    // 必要に応じてファイル監視を開始
    if let Some(ref fp) = file_path {
        let mut fw = FileWatcher::new();
        fw.watch(app.clone(), label.clone(), fp.clone());
        let watchers = app.state::<FileWatchers>();
        watchers.lock().unwrap().insert(label.clone(), fw);

        // 最近使ったファイルに追加
        let recent = app.state::<RecentState>();
        let mut store = recent.lock().unwrap();
        store.add(fp, &doc_title);
    }

    // 履歴追跡を開始
    {
        let history = app.state::<HistoryState>();
        let mut hs = history.lock().unwrap();
        hs.start_tracking(&label, &content, file_path.as_deref());
    }

    eprintln!("tsumugi: new window {} (instance: {})", label, instance_id);

    Ok(instance_id)
}

pub fn run() {
    // すべてのインスタンスが1つのタスクバーボタンを共有するように AppUserModelID を設定
    #[cfg(target_os = "windows")]
    set_app_user_model_id();

    let mut args = cli::CliArgs::parse();

    // --body - : 標準入力から読み取り（デーモン化の前に行う必要あり）
    let stdin_read = if args.body.as_deref() == Some("-") {
        use std::io::Read as _;
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .expect("tsumugi: failed to read stdin");
        args.body = Some(input);
        true
    } else {
        false
    };

    // 最初のウィンドウのインスタンスIDを決定
    let id = args.id.clone().unwrap_or_else(|| {
        if args.body.is_some() {
            let auto_id = format!("body-{:04x}", rand_u16());
            println!("{}", auto_id);
            auto_id
        } else if let Some(ref file) = args.file {
            file_to_id(file)
        } else if let Some(ref file) = args.file_pos {
            file_to_id(file)
        } else {
            format!("gui-{:04x}", rand_u16())
        }
    });

    // --id が指定された場合、既存のインスタンスへの接続を試みる
    if args.id.is_some() {
        if ipc::send_to_existing(&id, &args).is_ok() {
            return;
        }
        // 既存インスタンスが見つからない — 起動処理に fall through
    }

    // 既存インスタンスが必要な読み書き操作の場合、エラーで終了
    if !args.query.is_empty() || args.grep.is_some() || args.lines.is_some()
        || args.delete.is_some() || args.insert.is_some() || args.replace.is_some()
    {
        eprintln!("tsumugi: No instance found with id: {}", id);
        std::process::exit(2);
    }
    if args.list {
        ipc::list_instances();
        return;
    }

    // 既存のプライマリプロセスに委譲を試みる（そちらでウィンドウを作成）
    if args.id.is_none() {
        let file_arg = args.file.clone().or_else(|| args.file_pos.clone());
        if ipc::send_create_window(file_arg, args.body.clone(), args.title.clone()).is_ok() {
            return;
        }
        // プライマリプロセスが存在しない — 自身がプライマリになるよう fall through
    }

    // デーモン化: バックグラウンドで自身を再起動し、ターミナルを即座に解放する
    #[cfg(target_os = "macos")]
    let should_daemonize = !args.foreground && {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
            || args.body.is_some()
            || args.file.is_some()
            || args.file_pos.is_some()
    };
    #[cfg(not(target_os = "macos"))]
    let should_daemonize = !args.foreground && {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    };

    if should_daemonize {
        let exe = std::env::current_exe().expect("failed to get executable path");
        let mut child_args: Vec<String> = std::env::args().skip(1).collect();
        child_args.push("--_foreground".to_string());
        // 自動生成したIDを明示的に渡し、子プロセスで再生成されないようにする
        if args.id.is_none() {
            child_args.push("--id".to_string());
            child_args.push(id.clone());
        }
        use std::process::{Command, Stdio};

        // macOSでは `open` 経由で .app バンドルから起動し、
        // Dockに正しいアプリアイコンを表示させる。
        #[cfg(target_os = "macos")]
        let use_open = !stdin_read && find_app_bundle(&exe).is_some();
        #[cfg(not(target_os = "macos"))]
        let use_open = false;

        if use_open {
            #[cfg(target_os = "macos")]
            {
                let bundle = find_app_bundle(&exe).unwrap();
                let mut cmd = Command::new("open");
                cmd.arg("-n")
                    .arg("-a")
                    .arg(&bundle)
                    .arg("--args");
                cmd.args(&child_args);
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null());
                cmd.spawn().expect("failed to launch tsumugi via open");
            }
        } else {
            let mut cmd = Command::new(&exe);
            cmd.args(&child_args)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());

            if stdin_read {
                // 標準入力の内容を子プロセスにパイプする（子プロセスは --body - で読み取る）
                cmd.stdin(Stdio::piped());
                let mut child = cmd.spawn().expect("failed to launch tsumugi");
                if let Some(mut child_stdin) = child.stdin.take() {
                    use std::io::Write as _;
                    child_stdin
                        .write_all(args.body.as_deref().unwrap_or("").as_bytes())
                        .ok();
                }
            } else {
                cmd.stdin(Stdio::null());
                cmd.spawn().expect("failed to launch tsumugi");
            }
        }
        return;
    }

    let initial_content = args.body.clone().unwrap_or_default();
    let initial_title = args.title.clone().unwrap_or_else(|| "Untitled".to_string());
    let initial_file = args.file.clone().or_else(|| args.file_pos.clone());

    // ファイルモードの場合: コンテンツを事前に読み込んで状態に保存
    let (resolved_content, resolved_title, resolved_file_path) = if !initial_content.is_empty() {
        (initial_content.clone(), initial_title.clone(), None)
    } else if let Some(ref file_path) = initial_file {
        let abs_path = dunce::canonicalize(file_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(file_path));
        let abs_path_str = normalize_path(&abs_path.to_string_lossy());
        match commands::read_document(&abs_path) {
            Ok(content) => {
                let title = abs_path
                    .file_stem()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Untitled".to_string());
                (content, title, Some(abs_path_str))
            }
            Err(e) => {
                eprintln!("tsumugi: Failed to read file: {}", e);
                (String::new(), initial_title.clone(), None)
            }
        }
    } else {
        (String::new(), initial_title.clone(), None)
    };

    let content_explicitly_set = args.body.is_some() || initial_file.is_some();
    let mut app_state = WindowState::new(id.clone(), resolved_title, resolved_content);
    app_state.content_explicitly_set = content_explicitly_set;
    if let Some(ref fp) = resolved_file_path {
        app_state.saved_path = Some(fp.clone());
        app_state.path_disclosure = true;
    }

    // 初期状態を準備
    let mut initial_states = HashMap::new();
    initial_states.insert("main".to_string(), app_state);

    let id_for_setup = id.clone();
    let resolved_file_path_for_setup = resolved_file_path.clone();

    let i18n = i18n::I18n::new();

    // macOSのウィンドウ状態復元データを削除する。
    // 起動時にFinder経由のOpenDocumentsイベントとstate restorationが同時発生すると、
    // Tao内部のイベントコールバックで再入が起きクラッシュする問題を回避する。
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            let saved_state = home
                .join("Library/Saved Application State/com.tsumugi.app.savedState");
            let _ = std::fs::remove_dir_all(saved_state);
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .manage(Mutex::new(initial_states) as WindowStates)
        .manage(Mutex::new(HashMap::<String, FileWatcher>::new()) as FileWatchers)
        .manage(Mutex::new("main".to_string()) as LastFocusedDoc)
        .manage(TagState::new(TagStore::load()))
        .manage(RecentState::new(RecentStore::load()))
        .manage(HistoryState::new(HistoryStore::load()))
        .invoke_handler(tauri::generate_handler![
            commands::read_file,
            commands::save_file,
            commands::save_binary_file,
            commands::notify_saved,
            commands::get_saved_path,
            commands::sync_content,
            commands::record_history,
            commands::set_dirty,
            commands::get_initial_content,
            commands::rename_file,
            commands::get_translations,
            commands::get_platform,
            commands::execute_menu_action,
            commands::set_editor_menu_enabled,
            commands::open_new_window,
            commands::tag_add,
            commands::tag_remove,
            commands::tag_set,
            commands::tag_get,
            commands::tag_get_all,
            commands::tag_delete_entry,
            commands::tag_relink,
            commands::tag_set_memo,
            commands::tag_validate_paths,
            commands::tag_get_all_unique_tags,
            commands::tag_batch_add,
            commands::tag_rename_all,
            commands::tag_remove_all,
            commands::tag_get_counts,
            commands::get_custom_locale_path,
            commands::restart_app,
            commands::recent_get_all,
            commands::recent_add,
            commands::recent_remove,
            commands::recent_clear,
            pdf::export_pdf,
            pdf::has_pdf_browser,
            commands::history_get_config,
            commands::history_set_config,
            commands::history_get_files,
            commands::history_restore_at,
            commands::history_check_unsaved,
            commands::history_delete_file,
            commands::history_delete_files,
            commands::history_get_file_hash,
            commands::history_get_entries,
            commands::history_get_unsaved_diff,
            commands::history_delete_unsaved,
            commands::history_get_entry_previews,
            commands::history_get_entry_diff,
        ])
        .setup(move |app| {
            let menu = menu::build_menu(app.handle(), &i18n)?;
            app.set_menu(menu)?;

            // Windowsではカスタムタイトルバーのためにネイティブ装飾を無効化
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_decorations(false);
                }
            }

            app.manage(i18n::I18nState::new(i18n));

            // PDFコンテンツストアを初期化
            let pdf_store = std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::<String, String>::new(),
            ));
            app.manage(pdf_store.clone());

            // 共有HTTPサーバーを起動
            let (http_port, http_token) = http_api::start_http_server(app.handle().clone(), pdf_store);
            app.manage(http_api::HttpServerInfo { port: http_port, token: http_token.clone() });

            // 初期ウィンドウ用のHTTPポートファイルを書き込み
            let port_path = ipc::instance_file(&id_for_setup).with_extension("http");
            write_port_file(&port_path, &format!("{}:{}", http_port, http_token));
            eprintln!("tsumugi: HTTP API listening on http://127.0.0.1:{}", http_port);
            eprintln!("tsumugi: port file: {}", port_path.display());

            // 初期ウィンドウ用のウィンドウごとの IPC リスナーを開始
            ipc::start_listener(id_for_setup.clone(), "main".to_string(), app.handle().clone());

            // プライマリソケットリスナーを開始
            ipc::start_primary_listener(app.handle().clone());

            // 必要に応じて初期ウィンドウのファイル監視を開始
            if let Some(ref fp) = resolved_file_path_for_setup {
                let mut fw = FileWatcher::new();
                fw.watch(app.handle().clone(), "main".to_string(), fp.clone());
                let watchers = app.state::<FileWatchers>();
                watchers.lock().unwrap().insert("main".to_string(), fw);

                // 最近使ったファイルに追加
                let title = std::path::Path::new(fp)
                    .file_stem()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Untitled".to_string());
                let recent = app.state::<RecentState>();
                let mut store = recent.lock().unwrap();
                store.add(fp, &title);
            }

            // 初期ウィンドウの履歴追跡を開始
            {
                let history = app.state::<HistoryState>();
                let initial_content = {
                    let states = app.state::<WindowStates>();
                    let guard = states.lock().unwrap();
                    guard.get("main").map(|s| s.current_content.clone())
                };
                if let Some(content) = initial_content {
                    let mut hs = history.lock().unwrap();
                    hs.start_tracking("main", &content, resolved_file_path_for_setup.as_deref());
                }
            }

            // バックグラウンドで古い履歴エントリをクリーンアップ
            let app_handle_for_cleanup = app.handle().clone();
            std::thread::spawn(move || {
                let history = app_handle_for_cleanup.state::<HistoryState>();
                let mut hs = history.lock().unwrap();
                hs.cleanup_old_entries();
            });

            Ok(())
        })
        .on_menu_event(|app, event| {
            menu::handle_menu_event(app, event);
        })
        .on_window_event(|window, event| {
            let label = window.label().to_string();

            match event {
                // ドキュメントウィンドウとホームウィンドウのフォーカスを追跡
                tauri::WindowEvent::Focused(true) => {
                    if label == "main" || label.starts_with("doc-") || label.starts_with("main-home") {
                        let app = window.app_handle();
                        let last_focused = app.state::<LastFocusedDoc>();
                        *last_focused.lock().unwrap() = label;
                    }
                }
                // ウィンドウを閉じる前に未保存の変更を確認
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    // ドキュメントウィンドウのみ対象
                    if label != "main" && !label.starts_with("doc-") && !label.starts_with("main-home") {
                        return;
                    }

                    // ダーティ状態を確認
                    let is_dirty = {
                        let app = window.app_handle();
                        let states = app.state::<WindowStates>();
                        let guard = states.lock().unwrap();
                        guard.get(&label).map_or(false, |s| s.dirty)
                    };

                    if is_dirty {
                        api.prevent_close();

                        // 翻訳テキストを取得
                        let (msg, btn_save, btn_dont_save, btn_cancel) = {
                            let app = window.app_handle();
                            let i18n_state = app.state::<i18n::I18nState>();
                            let i18n = i18n_state.lock().unwrap();
                            (
                                i18n.t("ui.unsaved_confirm"),
                                i18n.t("ui.unsaved_save"),
                                i18n.t("ui.unsaved_dont_save"),
                                i18n.t("ui.unsaved_cancel"),
                            )
                        };

                        let window_clone = window.clone();
                        let save_label = btn_save.clone();
                        let dont_save_label = btn_dont_save.clone();
                        window.app_handle().dialog()
                            .message(&msg)
                            .title("tsumugi")
                            .kind(MessageDialogKind::Warning)
                            .buttons(MessageDialogButtons::YesNoCancelCustom(
                                btn_save, btn_dont_save, btn_cancel,
                            ))
                            .show_with_result(move |result| {
                                // YesNoCancelCustomではCustom(ボタンラベル)が返る
                                let is_save = matches!(&result, MessageDialogResult::Yes)
                                    || matches!(&result, MessageDialogResult::Custom(s) if s == &save_label);
                                let is_dont_save = matches!(&result, MessageDialogResult::No)
                                    || matches!(&result, MessageDialogResult::Custom(s) if s == &dont_save_label);
                                if is_save {
                                    // 「保存」→ フロントエンドに保存イベントをemitし、保存完了後に閉じる
                                    let _ = window_clone.emit("save-and-close", ());
                                } else if is_dont_save {
                                    // 「保存しない」→ そのまま閉じる
                                    let _ = window_clone.destroy();
                                }
                                // それ以外（キャンセル）→ 何もしない
                            });
                    }
                }
                // ウィンドウ破棄時のクリーンアップ
                tauri::WindowEvent::Destroyed => {
                    // ドキュメント以外のウィンドウはスキップ（aboutのみ）
                    if label != "main" && !label.starts_with("doc-") && !label.starts_with("main-home") {
                        // ただしaboutウィンドウの状態が存在する場合はクリーンアップ
                        if label == "about" {
                            let app = window.app_handle();
                            let states = app.state::<WindowStates>();
                            states.lock().unwrap().remove(&label);
                        }
                        return;
                    }

                    let app = window.app_handle();

                    // 状態を削除し、クリーンアップ用の instance_id を取得
                    let instance_id;
                    let should_exit;
                    {
                        let states = app.state::<WindowStates>();
                        let mut states = states.lock().unwrap();
                        instance_id = states.get(&label).map(|s| s.instance_id.clone());
                        states.remove(&label);
                        // ドキュメントウィンドウが残っているか確認
                        should_exit = !states.values().any(|s| {
                            // aboutウィンドウはドキュメントウィンドウではない
                            s.instance_id != "about"
                        });
                    }

                    // IPC ソケット、HTTPポートファイル、トークンファイルをクリーンアップ
                    if let Some(ref id) = instance_id {
                        let path = ipc::instance_file(id);
                        std::fs::remove_file(&path).ok();
                        std::fs::remove_file(path.with_extension("http")).ok();
                        std::fs::remove_file(path.with_extension("token")).ok();
                    }

                    // ファイル監視をクリーンアップ
                    {
                        let watchers = app.state::<FileWatchers>();
                        watchers.lock().unwrap().remove(&label);
                    }

                    // 履歴追跡を停止
                    {
                        let history = app.state::<HistoryState>();
                        let mut hs = history.lock().unwrap();
                        hs.stop_tracking(&label);
                    }

                    // ドキュメントウィンドウが残っていなければアプリを終了
                    if should_exit {
                        app.exit(0);
                    }
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_app_handle, event| {
            // macOSファイル関連付け: ファイルを開く
            // macOSのstate restorationコールバック中にOpenDocumentsイベントが配送されると、
            // Tao内部のイベントハンドラで再入が起きクラッシュする。
            // 別スレッドからrun_on_main_threadでイベントキューにポストすることで、
            // 現在のコールバックチェーンの外で安全に処理する。
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &event {
                let urls = urls.clone();
                let handle = _app_handle.clone();
                std::thread::spawn(move || {
                    let h = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        for url in &urls {
                            if let Ok(path) = url.to_file_path() {
                                let path_str = path.to_string_lossy().to_string();
                                let _ = open_document_window(&h, Some(path_str), None, None);
                            }
                        }
                    });
                });
            }
            match event {
                tauri::RunEvent::ExitRequested { api, code, .. } => {
                    if code.is_none() {
                        api.prevent_exit();
                    }
                }
                tauri::RunEvent::Exit => {
                    // プライマリソケットとトークンファイルをクリーンアップ
                    let primary_path = ipc::instance_file("tsumugi-primary");
                    std::fs::remove_file(&primary_path).ok();
                    std::fs::remove_file(primary_path.with_extension("token")).ok();
                }
                _ => {}
            }
        });
}

fn write_port_file(path: &std::path::Path, content: &str) {
    std::fs::write(path, content).ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
}

fn file_to_id(file: &str) -> String {
    let canonical = dunce::canonicalize(file)
        .unwrap_or_else(|_| std::path::PathBuf::from(file));
    let path_str = canonical.to_string_lossy();
    let hash = stable_hash::fnv1a64(path_str.as_bytes());
    let name = std::path::Path::new(file)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
        .to_lowercase()
        .replace('.', "-")
        .replace(' ', "-");
    format!("file-{}-{:016x}", name, hash)
}

/// ウィンドウを確実に手前に表示する。
/// Windowsでは SetForegroundWindow のOS制限を AttachThreadInput で回避する。
#[cfg(target_os = "windows")]
fn force_foreground(window: &tauri::WebviewWindow) {
    let _ = window.unminimize();

    let hwnd = match window.hwnd() {
        Ok(h) => h.0 as isize,
        Err(_) => {
            let _ = window.set_focus();
            return;
        }
    };

    unsafe {
        #[link(name = "user32")]
        extern "system" {
            fn GetForegroundWindow() -> isize;
            fn GetWindowThreadProcessId(hwnd: isize, process_id: *mut u32) -> u32;
            fn AttachThreadInput(attach: u32, attach_to: u32, fAttach: i32) -> i32;
            fn SetForegroundWindow(hwnd: isize) -> i32;
            fn BringWindowToTop(hwnd: isize) -> i32;
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThreadId() -> u32;
        }

        let foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());

        if current_thread != foreground_thread && foreground_thread != 0 {
            AttachThreadInput(current_thread, foreground_thread, 1);
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
            AttachThreadInput(current_thread, foreground_thread, 0);
        } else {
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
        }
    }

    let _ = window.set_focus();
}

/// macOS版: NSWindow APIを直接呼び、対象ウィンドウだけを手前に表示する。
/// set_focus() は NSApplication.activateIgnoringOtherApps を呼ぶため、
/// アプリの全ウィンドウが手前に来てしまう問題を回避する。
/// spawn_blocking 等のバックグラウンドスレッドから呼ばれる場合があるため、
/// performSelectorOnMainThread でメインスレッドにディスパッチする。
#[cfg(target_os = "macos")]
fn force_foreground(window: &tauri::WebviewWindow) {
    use raw_window_handle::HasWindowHandle;
    let _ = window.unminimize();

    unsafe {
        use objc2::msg_send;
        use objc2::runtime::{AnyObject, Bool};
        use objc2::sel;
        use raw_window_handle::RawWindowHandle;

        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                let ns_view = appkit.ns_view.as_ptr() as *const AnyObject;
                let ns_window: *const AnyObject = msg_send![ns_view, window];
                if !ns_window.is_null() {
                    let nil: *const AnyObject = std::ptr::null();
                    let () = msg_send![
                        ns_window,
                        performSelectorOnMainThread: sel!(orderFrontRegardless),
                        withObject: nil,
                        waitUntilDone: Bool::NO
                    ];
                    let () = msg_send![
                        ns_window,
                        performSelectorOnMainThread: sel!(makeKeyWindow),
                        withObject: nil,
                        waitUntilDone: Bool::NO
                    ];
                }
                return;
            }
        }
    }

    // フォールバック
    let _ = window.set_focus();
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn force_foreground(window: &tauri::WebviewWindow) {
    let _ = window.unminimize();
    let _ = window.set_focus();
}

fn rand_u16() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::getrandom(&mut buf).expect("failed to generate random bytes");
    u16::from_le_bytes(buf)
}

/// 実行ファイルのパスから親方向に辿り、囲んでいる .app バンドルを探す。
#[cfg(target_os = "macos")]
fn find_app_bundle(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut path = exe.to_path_buf();
    loop {
        if path.extension().map_or(false, |ext| ext == "app") {
            return Some(path);
        }
        if !path.pop() {
            return None;
        }
    }
}

/// すべての tsumugi インスタンスが1つのタスクバーボタンを共有するように AppUserModelID を設定する。
#[cfg(target_os = "windows")]
fn set_app_user_model_id() {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    extern "system" {
        fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
    }

    let id: Vec<u16> = std::ffi::OsStr::new("com.tsumugi.app")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}
