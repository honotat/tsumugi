use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{command, Manager as _};

// PDFプロファイルを一意化するプロセス内カウンター
static PDF_PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

// スコープを抜けるときに一時プロファイルを削除する
struct ProfileCleanup<'a>(&'a Path);
impl Drop for ProfileCleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

use crate::commands::validate_path;
use crate::http_api::{HttpServerInfo, PdfContentStore};

/// macOS: Chromiumベースのブラウザを探索する
#[cfg(target_os = "macos")]
fn find_browser() -> Option<PathBuf> {
    let candidates = [
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Windows: Chromiumベースのブラウザを探索する
#[cfg(target_os = "windows")]
fn find_browser() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// ブラウザのheadlessモードでHTMLをPDFに変換する
fn generate_pdf_with_browser(
    browser: &Path,
    html_url: &str,
    output_path: &Path,
) -> Result<(), String> {
    let output_arg = format!("--print-to-pdf={}", output_path.display());

    // 既存Edgeと競合しない一意な一時プロファイル
    let seq = PDF_PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let profile_dir = std::env::temp_dir()
        .join(format!("tsumugi-pdf-profile-{}-{}", std::process::id(), seq));
    let user_data_arg = format!("--user-data-dir={}", profile_dir.display());
    let _cleanup = ProfileCleanup(&profile_dir);

    // --no-pdf-header-footer を使用（新しいChromiumバージョン向け）
    let result = Command::new(browser)
        .args([
            "--headless=old",
            "--disable-gpu",
            "--no-pdf-header-footer",
            &user_data_arg,
            &output_arg,
            html_url,
        ])
        .output()
        .map_err(|e| format!("Failed to launch browser: {}", e))?;

    // stderrにネットワークエラーが含まれていたらエラーとして返す
    let stderr = String::from_utf8_lossy(&result.stderr);
    if stderr.contains("ERR_") || stderr.contains("net::") {
        return Err(format!("PDF generation failed: {}", stderr));
    }

    if output_path.exists() {
        return Ok(());
    }

    // フォールバック: --print-to-pdf-no-header（古いChromium互換）
    let result = Command::new(browser)
        .args([
            "--headless=old",
            "--disable-gpu",
            "--print-to-pdf-no-header",
            &user_data_arg,
            &output_arg,
            html_url,
        ])
        .output()
        .map_err(|e| format!("Failed to launch browser: {}", e))?;

    let stderr = String::from_utf8_lossy(&result.stderr);
    if stderr.contains("ERR_") || stderr.contains("net::") {
        return Err(format!("PDF generation failed: {}", stderr));
    }

    if output_path.exists() {
        Ok(())
    } else {
        Err(format!("PDF generation failed: {}", stderr))
    }
}

/// macOS: WKWebViewベースのsidecarでPDFを生成する
#[cfg(target_os = "macos")]
async fn generate_pdf_with_sidecar(
    app: &tauri::AppHandle,
    html_url: &str,
    output_path: &Path,
) -> Result<(), String> {
    use tauri_plugin_shell::ShellExt;

    let output = app
        .shell()
        .sidecar("tsumugi-pdf")
        .map_err(|e| format!("Failed to find sidecar: {}", e))?
        .args([
            html_url,
            output_path.to_string_lossy().as_ref(),
        ])
        .output()
        .await
        .map_err(|e| format!("Sidecar execution failed: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("PDF generation failed (sidecar): {}", stderr))
    }
}

/// ブラウザが見つからない場合のフォールバック
#[cfg(target_os = "macos")]
async fn pdf_fallback(
    app: &tauri::AppHandle,
    html_url: &str,
    output_path: &Path,
) -> Result<(), String> {
    generate_pdf_with_sidecar(app, html_url, output_path).await
}

#[cfg(not(target_os = "macos"))]
async fn pdf_fallback(
    _app: &tauri::AppHandle,
    _html_url: &str,
    _output_path: &Path,
) -> Result<(), String> {
    Err("No Chromium-based browser found. Please install Chrome or Edge.".to_string())
}

#[command]
pub async fn export_pdf(
    app: tauri::AppHandle,
    html_content: String,
    output_path: String,
) -> Result<(), String> {
    let output = validate_path(&output_path)?;

    // HTTPサーバー経由でHTMLを配信する
    let http_info = app.state::<HttpServerInfo>();
    let port = http_info.port;

    let token = crate::http_api::generate_token();
    {
        let store = app.state::<PdfContentStore>();
        store.lock().unwrap().insert(token.clone(), html_content);
    }

    let html_url = format!("http://127.0.0.1:{}/pdf-content/{}", port, token);

    // ブラウザでPDF生成を試行
    let result = if let Some(browser) = find_browser() {
        generate_pdf_with_browser(&browser, &html_url, &output)
    } else {
        // フォールバック（macOS: sidecar / Windows: エラー）
        pdf_fallback(&app, &html_url, &output).await
    };

    // 消費されなかったケースのクリーンアップ
    {
        let store = app.state::<PdfContentStore>();
        store.lock().unwrap().remove(&token);
    }

    result
}

#[command]
pub fn has_pdf_browser() -> bool {
    // macOSではsidecarフォールバックがあるため常にtrue
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        find_browser().is_some()
    }
}
