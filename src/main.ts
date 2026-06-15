import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { open } from '@tauri-apps/plugin-dialog';
import { ThemeManager } from './theme';
import { handleSave, handleSaveAs } from './save';
import { FindBar } from './find';
import { FontSizeManager } from './font-size';
import { CustomTitleBar } from './titlebar';
import { EditorController } from './editor/editor-controller';
import { StatusBar, SPLIT_MIN_WIDTH } from './status-bar';
import { getMarkdownIt, sanitizeHtml } from './renderer';
import { exportAsPdf } from './pdf-export';
import { exportAsHtml } from './html-export';
import { TagAddModal } from './tag-add-modal';
import { TagSidebar } from './tag-sidebar';
import { HomeScreen } from './home';
import { UpdateModal } from './update-modal';
import { HistorySettingsModal, UnsavedDiffResult } from './history-settings-modal';
import { loadTranslations } from './i18n';

interface ContentUpdate {
  body?: string;
  title?: string;
}

interface MenuAction {
  action: string;
  value?: string | boolean;
}

let currentContent = '';
let currentTitle = 'Untitled';
let customTitleBar: CustomTitleBar | null = null;

let isHome = false;
let isEditor = false;

let themeManager: ThemeManager;
let findBar: FindBar;
let fontSizeManager: FontSizeManager;
let editorController: EditorController;
let statusBar: StatusBar;
let tagAddModal: TagAddModal;
let tagSidebar: TagSidebar;
let updateModal: UpdateModal;
let historySettingsModal: HistorySettingsModal;
let homeScreen: HomeScreen | null = null;

// 初期化
(async () => {

themeManager = new ThemeManager();
updateModal = new UpdateModal();
historySettingsModal = new HistorySettingsModal();

// エディタコンポーネントを常に初期化（HomeScreen.init()がエディタ要素を非表示にする）
findBar = new FindBar();
fontSizeManager = new FontSizeManager();
editorController = new EditorController(document.getElementById('content')!);
statusBar = new StatusBar();
tagAddModal = new TagAddModal();
tagSidebar = new TagSidebar();

tagAddModal.onTagAdded(() => {
  tagSidebar.refresh();
});

findBar.setOnReplace((search, replace, all) => {
  let content = editorController.getCurrentContent();
  if (all) {
    const regex = new RegExp(search.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
    content = content.replace(regex, replace);
  } else {
    const idx = content.toLowerCase().indexOf(search.toLowerCase());
    if (idx === -1) return;
    content = content.slice(0, idx) + replace + content.slice(idx + search.length);
  }
  editorController.updateContent(content);
  currentContent = content;
  invoke('sync_content', { content });
  findBar.search();
  scheduleHistoryRecord();
});

// 履歴記録用デバウンス（操作停止後2秒で記録）
let historyDebounceTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleHistoryRecord() {
  if (historyDebounceTimer !== null) {
    clearTimeout(historyDebounceTimer);
  }
  historyDebounceTimer = setTimeout(() => {
    invoke('record_history');
    historyDebounceTimer = null;
  }, 2000);
}

editorController.setOnContentChange((markdown) => {
  currentContent = markdown;
  invoke('sync_content', { content: markdown });
  statusBar.update(markdown);
  scheduleHistoryRecord();
});

statusBar.setOnModeChange((mode) => {
  if (!isEditor) return;
  if (mode === 'view') editorController.switchToView();
  else if (mode === 'edit') editorController.switchToEdit();
  else editorController.switchToSplit();
});

editorController.setOnModeChange((mode) => {
  statusBar.setActiveTab(mode);
});

// リサイズ時にSplitタブの表示/非表示を制御し、狭い画面ではEditに自動切替
function handleResize(): void {
  if (!isEditor) return;
  statusBar.updateSplitTabVisibility();
  if (window.innerWidth < SPLIT_MIN_WIDTH && statusBar.getActiveMode() === 'split') {
    editorController.switchToEdit();
  }
}
window.addEventListener('resize', handleResize);
handleResize();

function updateWindowTitle(title: string) {
  getCurrentWindow().setTitle(`${title} — tsumugi`);
  customTitleBar?.setTitle(title);
}

// ファイル選択ダイアログを開き、新しいウィンドウで開く
async function openFileInNewWindow() {
  const selected = await open({
    multiple: false,
    filters: [{ name: 'Markdown', extensions: ['md', 'markdown', 'txt'] }],
  });
  if (selected) {
    await invoke('open_new_window', { file: selected as string });
  }
}

async function doSave() {
  currentContent = editorController.getCurrentContent();
  await handleSave(currentContent, currentTitle);
}

async function doSaveAs() {
  currentContent = editorController.getCurrentContent();
  await handleSaveAs(currentContent, currentTitle);
}

async function copyAsMarkdown() {
  currentContent = editorController.getCurrentContent();
  await navigator.clipboard.writeText(currentContent);
}

async function copyAsHtml() {
  currentContent = editorController.getCurrentContent();
  const md = getMarkdownIt();
  const html = sanitizeHtml(md.render(currentContent));
  await navigator.clipboard.writeText(html);
}

async function copyAsPlaintext() {
  const text = document.getElementById('content')!.textContent || '';
  await navigator.clipboard.writeText(text);
}

async function reloadCurrentFile() {
  const savedPath = await invoke<string | null>('get_saved_path');
  if (!savedPath) return;
  try {
    const content = await invoke<string>('read_file', { path: savedPath });
    currentContent = content;
    editorController.updateContent(content);
    await invoke('sync_content', { content });
    invoke('set_dirty', { dirty: false });
  } catch (e) {
    console.error('Reload failed:', e);
  }
}

// ネイティブメニューと JS ハンドラの二重発火を防ぐデバウンスガード
const actionDebounce = new Set<string>();
function debounced(action: string, fn: () => void) {
  if (actionDebounce.has(action)) return;
  actionDebounce.add(action);
  setTimeout(() => actionDebounce.delete(action), 300);
  fn();
}

function applyTranslations(): void {
  if (isEditor) {
    statusBar.applyTranslations();
    findBar.applyTranslations();
    tagAddModal.applyTranslations();
    tagSidebar.applyTranslations();
  }
  historySettingsModal.applyTranslations();
  if (homeScreen) {
    homeScreen.applyTranslations();
  }
}

// content_explicitly_setを見てHome UIかEditor UIかを動的に決定
async function loadInitialContent() {
  await loadTranslations();
  const [body, title, contentSet] = await invoke<[string, string, boolean]>('get_initial_content');

  if (!contentSet) {
    isHome = true;
    isEditor = false;
    homeScreen = new HomeScreen();
    await homeScreen.init();
    // ホーム画面ではエディタ専用メニュー項目を無効化する
    await invoke('set_editor_menu_enabled', { enabled: false });
  } else {
    isHome = false;
    isEditor = true;
    currentContent = body;
    currentTitle = title || 'Untitled';
    editorController.enterEditMode(body);
    updateWindowTitle(currentTitle);
    statusBar.update(body);
    statusBar.updateSplitTabVisibility();

    // 未保存の変更履歴があれば差分確認モーダルを表示
    const savedPath = await invoke<string | null>('get_saved_path');
    if (savedPath) {
      try {
        const fileHash = await invoke<string>('history_get_file_hash', { path: savedPath });
        const diff = await invoke<UnsavedDiffResult | null>('history_get_unsaved_diff', { fileHash });
        if (diff) {
          historySettingsModal.showUnsavedDiffModal(fileHash, diff, {
            onDiscard: () => {},
            onOpenTemp: (content) => { invoke('open_new_window', { body: content }); },
            onSave: async (content) => {
              editorController.updateContent(content);
              currentContent = content;
              await invoke('sync_content', { content });
              await doSave();
            },
          });
        }
      } catch { /* 履歴チェック失敗は無視 */ }
    }
  }
  applyTranslations();
}
await loadInitialContent();

// Windows でカスタムタイトルバーを初期化する（loadInitialContent後に実行し、isHomeが確定済み）
async function initPlatformUI() {
  const platform = await invoke<string>('get_platform');
  if (platform === 'windows') {
    customTitleBar = new CustomTitleBar();
    await customTitleBar.init();
    if (isHome) {
      customTitleBar.setTitle('');
      customTitleBar.disableMaximize();
      customTitleBar.setEditorMenuEnabled(false);
    } else if (currentTitle !== 'Untitled') {
      customTitleBar.setTitle(currentTitle);
    }
  }
}
initPlatformUI();

// content-update リスナー（Home/Editor両対応）
listen('content-update', async (event) => {
  const update = event.payload as ContentUpdate;
  if (isHome && update.body !== undefined) {
    // Home → Editor遷移
    isHome = false;
    isEditor = true;
    if (homeScreen) { homeScreen.destroy(); homeScreen = null; }
    currentContent = update.body;
    currentTitle = update.title || 'Untitled';
    editorController.enterEditMode(currentContent);
    updateWindowTitle(currentTitle);
    statusBar.update(currentContent);
    statusBar.updateSplitTabVisibility();
    applyTranslations();
    // エディタ専用メニュー項目を有効化する
    invoke('set_editor_menu_enabled', { enabled: true });
    customTitleBar?.setEditorMenuEnabled(true);
    return;
  }
  // 既存のEditor更新ロジック
  if (update.body !== undefined) {
    currentContent = update.body;
    editorController.updateContent(update.body);
    statusBar.update(update.body);
  }
  if (update.title !== undefined) {
    currentTitle = update.title;
    updateWindowTitle(currentTitle);
  }
});

// save-and-close リスナー: Rust側から保存→閉じるフローを要求された時
listen('save-and-close', async () => {
  if (isEditor) {
    await doSave();
  }
  getCurrentWindow().close();
});

// switch-to-tags-tab リスナー（常に登録）
listen('switch-to-tags-tab', () => {
  if (homeScreen) {
    homeScreen.switchToTagsTab();
  }
});

// 統合menu-actionリスナー
listen('menu-action', (event) => {
  const { action, value } = event.payload as MenuAction;

  // Home/Editor共通のアクション
  switch (action) {
    case 'theme_change':
      if (value === 'dark' || value === 'light' || value === 'auto') {
        themeManager.setTheme(value);
      }
      return;
    case 'help_check_updates':
      debounced('help_check_updates', () => updateModal.checkForUpdates(false));
      return;
    case 'locale_changed':
      (async () => {
        await loadTranslations();
        applyTranslations();
        if (isEditor) statusBar.update(currentContent);
      })();
      return;
    case 'file_new_window':
      debounced('file_new_window', () => invoke('open_new_window', { file: null, body: '' }));
      return;
    case 'file_open':
      debounced('file_open', () => openFileInNewWindow());
      return;
    case 'file_history_settings':
      debounced('file_history_settings', () => historySettingsModal.show());
      return;
  }

  // Editor専用アクション
  if (!isEditor) return;

  switch (action) {
    case 'file_save':
      debounced('file_save', () => doSave());
      break;
    case 'file_save_as':
      debounced('file_save_as', () => doSaveAs());
      break;
    case 'file_reload':
      debounced('file_reload', () => reloadCurrentFile());
      break;
    case 'file_export_pdf':
      debounced('file_export_pdf', () => {
        currentContent = editorController.getCurrentContent();
        exportAsPdf(currentTitle, currentContent);
      });
      break;
    case 'file_export_html':
      debounced('file_export_html', () => {
        currentContent = editorController.getCurrentContent();
        exportAsHtml(currentTitle, currentContent);
      });
      break;
    case 'file_print':
      debounced('file_print', () => window.print());
      break;
    case 'edit_copy_markdown':
      debounced('edit_copy_markdown', () => copyAsMarkdown());
      break;
    case 'edit_copy_html':
      debounced('edit_copy_html', () => copyAsHtml());
      break;
    case 'edit_copy_plaintext':
      debounced('edit_copy_plaintext', () => copyAsPlaintext());
      break;
    case 'edit_find':
      debounced('edit_find', () => {
        if (findBar.isVisible()) {
          findBar.hide();
        } else {
          findBar.show();
        }
      });
      break;
    case 'edit_find_replace':
      debounced('edit_find_replace', () => {
        if (findBar.isReplaceVisible()) {
          findBar.hide();
        } else {
          findBar.showReplace();
        }
      });
      break;
    case 'edit_find_next':
      findBar.show();
      findBar.next();
      break;
    case 'edit_find_prev':
      findBar.show();
      findBar.prev();
      break;
    case 'font_increase':
      debounced('font_increase', () => fontSizeManager.increase());
      break;
    case 'font_decrease':
      debounced('font_decrease', () => fontSizeManager.decrease());
      break;
    case 'tag_add':
      debounced('tag_add', () => {
        if (tagSidebar.isVisible()) {
          tagSidebar.focusInput();
        } else {
          tagAddModal.show();
        }
      });
      break;
    case 'tag_edit':
      debounced('tag_edit', () => tagSidebar.toggle());
      break;
  }
});

document.addEventListener('dragover', (e) => {
  if (!isEditor) return;
  e.preventDefault();
  e.stopPropagation();
});

document.addEventListener('drop', async (e) => {
  if (!isEditor) return;
  e.preventDefault();
  e.stopPropagation();
  const file = e.dataTransfer?.files[0];
  if (file && (file.name.endsWith('.md') || file.name.endsWith('.markdown') || file.name.endsWith('.txt'))) {
    const text = await file.text();
    currentContent = text;
    currentTitle = file.name.replace(/\.[^.]+$/, '');
    editorController.updateContent(text);
    statusBar.update(text);
    updateWindowTitle(currentTitle);
  }
});

document.addEventListener('keydown', (e) => {
  if (!isEditor) return;

  if (e.key === 'Escape') {
    if (findBar.isVisible()) {
      findBar.hide();
    }
    return;
  }

  const mod = e.metaKey || e.ctrlKey;
  if (!mod) return;

  const inTextarea = document.activeElement?.tagName === 'TEXTAREA' || document.activeElement?.tagName === 'INPUT';

  switch (e.key) {
    case 'z':
      if (!inTextarea) {
        e.preventDefault();
        if (e.shiftKey) {
          editorController.redo();
        } else {
          editorController.undo();
        }
      }
      break;
    case 'n':
      e.preventDefault();
      debounced('file_new_window', () => invoke('open_new_window', { file: null, body: '' }));
      break;
    case 'o':
      e.preventDefault();
      debounced('file_open', () => openFileInNewWindow());
      break;
    case 's':
      e.preventDefault();
      if (e.shiftKey) {
        debounced('file_save_as', () => doSaveAs());
      } else {
        debounced('file_save', () => doSave());
      }
      break;
    case 'r':
      e.preventDefault();
      debounced('file_reload', () => reloadCurrentFile());
      break;
    case 'e':
    case 'E':
      if (e.shiftKey) {
        e.preventDefault();
        debounced('file_export_pdf', () => {
          currentContent = editorController.getCurrentContent();
          exportAsPdf(currentTitle, currentContent);
        });
      }
      break;
    case 'p':
      e.preventDefault();
      debounced('file_print', () => window.print());
      break;
    case 'f':
      if (!inTextarea) {
        e.preventDefault();
        debounced('edit_find', () => {
          if (findBar.isVisible()) {
            findBar.hide();
          } else {
            findBar.show();
          }
        });
      }
      break;
    case 'h':
      if (!inTextarea) {
        e.preventDefault();
        debounced('edit_find_replace', () => {
          if (findBar.isReplaceVisible()) {
            findBar.hide();
          } else {
            findBar.showReplace();
          }
        });
      }
      break;
    case 'g':
      e.preventDefault();
      if (e.shiftKey) {
        findBar.show();
        findBar.prev();
      } else {
        findBar.show();
        findBar.next();
      }
      break;
    case 'C':
    case 'c':
      if (e.shiftKey && !inTextarea) {
        e.preventDefault();
        debounced('edit_copy_markdown', () => copyAsMarkdown());
      }
      break;
    case 't':
      if (!inTextarea) {
        e.preventDefault();
        debounced('tag_add', () => {
          if (tagSidebar.isVisible()) {
            tagSidebar.focusInput();
          } else {
            tagAddModal.show();
          }
        });
      }
      break;
    case '=':
      e.preventDefault();
      debounced('font_increase', () => fontSizeManager.increase());
      break;
    case '-':
      e.preventDefault();
      debounced('font_decrease', () => fontSizeManager.decrease());
      break;
  }
});

})();
