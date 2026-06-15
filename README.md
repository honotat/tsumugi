# tsumugi

[![GitHub Release](https://img.shields.io/github/v/release/HonotaKobo/mdcast?style=flat-square)](https://github.com/HonotaKobo/mdcast/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey?style=flat-square)](https://github.com/HonotaKobo/mdcast/releases)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-FFC131?style=flat-square&logo=tauri&logoColor=white)](https://tauri.app)

AIエージェントが作り、人が確認する。そのための Markdown エディタ。

**本ソフトウェアは開発途上の未完成品です。** 仕様や記法、設定ファイルの形式は予告なく変更されることがあり、バージョンアップで後方互換性のない破壊的変更が入る場合があります。

## どのようなエディタか

AIエージェントを使って作業をしていると、エージェントがまとめた情報を確認する場面が頻繁にあります。その表示先は、Claude Desktop などのチャット画面だったり、ターミナル上だったり、一時的に生成された Markdown ファイルだったり様々ですが、どの方法にも不便な点があります。

チャット画面やターミナルに表示された内容は、やり取りが続くとすぐに埋もれてしまいます。内容に修正があればゼロから書き直しになり、後でファイルとして残したければコピー＆ペーストで自分で保存する必要があります。一方、ファイルとして出力すれば別画面で参照しながら作業でき、部分的な修正もできますが、不要になったファイルを整理しないとフォルダがすぐに散らかります。

tsumugi は、こうした問題のいいとこ取りを目指して作りました。

AIエージェントが MCP サーバー経由で Markdown の内容をウィンドウに表示するので、一時ファイルを作る必要がありません。内容の確認が済んでファイルとして保存する必要がなければ、ウィンドウを閉じるだけで終わりです。保存したければ、ウィンドウ上の保存ボタンからいつでもファイルに書き出せます。ファイルとして保存していない状態でも、AIエージェントは MCP 経由でウィンドウ上の内容を自由に更新できます。

AIエージェントとのやり取りをよりスムーズにするために、独自のカスタム記法にも対応しています。

## 想定する使い方

AIエージェントが文章を生成しながら、同じウィンドウに逐次反映していく流れを想定しています。MCP サーバーを導入すると、自然言語で指示するだけで tsumugi が操作されます。

```
「tsumugi に調査結果をまとめて表示して」
→ launch ツールで新しいウィンドウが開き、Markdown が表示される

「内容を更新して」
→ update ツールで既存ウィンドウの内容が更新される

「10〜20 行目だけ書き換えて」
→ edit_replace ツールで指定行だけが置換される

「内容を確認して」
→ query ツールで現在の本文が取得される
```

ウィンドウごとにインスタンス ID が自動的に割り振られ、複数のウィンドウを同時に扱うこともできます。

MCP サーバーの導入方法は [mcp-server/MCP-SERVER.md](mcp-server/MCP-SERVER.md) を参照してください。

もちろん、AIエージェントを使わずに普通の Markdown ビューアとしても使えます。

```bash
tsumugi README.md          # ファイルを開く
tsumugi                    # GUIファイル選択ダイアログから開く
```

## インストール

### macOS（Homebrew）

```bash
brew tap HonotaKobo/tsumugi
brew install --cask tsumugi
```

### Windows（Scoop）

```powershell
scoop bucket add tsumugi https://github.com/HonotaKobo/scoop-tsumugi
scoop install tsumugi
```

Scoop が未インストールの場合は、先にインストールしてください。

[![Scoop](https://img.shields.io/badge/Scoop-scoop.sh-blue?style=flat-square)](https://scoop.sh/)

```powershell
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
Invoke-RestMethod -Uri https://get.scoop.sh | Invoke-Expression
```

### その他

GitHub Releases から最新版をダウンロードしてください。

[![GitHub Release](https://img.shields.io/github/v/release/HonotaKobo/mdcast?style=flat-square)](../../releases)

| プラットフォーム | フォーマット |
|----------|--------|
| macOS | `.app` (tar.gz) |
| Windows | `.msi`, `.exe` |

## 使い方

### Markdown ビューアとして

```bash
tsumugi README.md                    # ファイルを開く
tsumugi                              # GUIファイル選択ダイアログから開く
```

### AIエージェントから（MCP サーバー）

MCP サーバーを登録すると、Claude Code / Claude Desktop / Codex CLI から tsumugi を操作できます。

導入方法・使えるツールの一覧は [mcp-server/MCP-SERVER.md](mcp-server/MCP-SERVER.md) を参照してください。

## タグ管理

ファイルにタグを付けて管理できます。フォルダ構成に縛られずにファイルを整理できるため、散らかったファイルもタグで横断的に検索・分類できます。

- **タグの追加**: `Ctrl+T`（macOS は `Cmd+T`）でタグをすばやく追加
- **タグの編集**: サイドバーで現在のファイルに付いたタグを一覧・追加・削除
- **タグマネージャー**: すべてのタグ付きファイルを一覧表示し、ファイル名やタグ名で検索


タグはファイルの内容とは独立して `~/.config/tsumugi/tags.json` に保存されるため、ファイルを移動・削除しても安全です。ファイルパスが変わった場合はタグマネージャーからリンクの修正や削除ができます。

## ライセンス

MIT