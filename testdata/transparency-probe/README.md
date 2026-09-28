# 背景だけを半透明にする検証（2026-09-17）

Windows上で、Slint 1.17.1の本文背景だけを透かし、文字と操作欄を不透明に描く小さな実行プログラム。
Editor本体への組み込みはしていない。Editorの設定・原稿・セッションには触れない。

## 起動とビルド

ビルド済みの実行ファイルは `../../target/release/transparency_probe.exe`。
再ビルドはEditorフォルダからPowerShellで次を実行する。

```powershell
& .\testdata\transparency-probe\prepare.ps1
```

手元のCargoキャッシュにある `i-slint-backend-winit-1.17.1` を
`target/transparency-probe/backend-winit` へ複製し、ソフトウェア描画の画面出力だけを
`sw_layered.rs` で置き換える。Cargoのpatch指定はこのビルドコマンドだけに適用する。
本体のCargo.toml、依存ライブラリの元キャッシュは変更しない。ネットワークは使用しない。
専用Cargo.lockを含む。Windows x64・既存のRust/Cargo環境で検証した。

## 操作

- 青地に黄色い帯が動く別ウィンドウを背後に表示する。
- スライダーで本文背景の不透明度を10〜100%に変える。検証用の範囲であり、製品仕様ではない。
- 入力欄で日本語入力とIME変換を試す。下の本文はホイールでスクロールできる。
- 上端の濃い帯をドラッグして移動する。
- 「サイズ」で760×520と1050×700の物理画素サイズを切り替える。
- 「計測」で表示カウンターを16ms間隔・180回更新する。
- 「終了」で両方の検証ウィンドウを閉じる。

## 仕組み

SlintのSoftwareRendererで乗算済みRGBAを描き、BGRAの32bit DIBへ変換して
`UpdateLayeredWindow(ULW_ALPHA)` へ渡す。`SourceConstantAlpha=255`、
`AlphaFormat=AC_SRC_ALPHA` として各画素の透明度を使用する。
背景の白だけに透明度を指定し、文字と操作欄は不透明色で描く。
入力、アクセシビリティ、IMEは既存のSlint/winitの経路を利用する。

winitが表示・サイズ変更時にウィンドウスタイルを再設定するため、描画直前に
`WS_EX_LAYERED` が落ちていれば付け直す。これがない初版は2回目以降の
`UpdateLayeredWindow` がエラー87で失敗した。
`SetLayeredWindowAttributes` は使わない。
ウィンドウの初期寸法はSlintの`preferred-width/height`で指定する。
`width/height`を定数指定した初版ではネイティブ窓を広げても内容が初期寸法に残った。

## 確認結果と限界

実画面で確認済み：背景越しに別ウィンドウの黄色い帯が動く、背景不透明度65%と24%の切替、100%で背後が隠れること、
操作欄・本文文字の濃さ維持、日本語文字列の入力、IME変換中表示・候補一覧・確定、
本文スクロール、ドラッグ移動、サイズ変更後の描画領域の追従。
IMEはこの環境のかな入力で「の」から「乃」への変換・確定を確認した。

ログは実行ファイルと同じ場所の `transparency_probe.log`。起動時に前回分を置き換える。
初回・サイズ変更時に半透明画素数と不透明画素数を記録する。
60描画ごとに描画・BGRA変換・UpdateLayeredWindow呼出しの合計時間を出す。
これは入力から表示までの遅延やDWMでの実際の画面提示時間ではない。
初期化、IME・キャレット点滅・操作による描画もサンプルに含むため、厳密なベンチマークではない。
サイズ変更をまたぐ最初の60描画の集計には変更前のサンプルも混ざるので、比較には使用しない。
最終検証の1050×700で、サイズ変更後の最初の集計を除くと中央値0.65〜0.83ms、
95パーセンタイル0.90〜1.25msだった。記録の控えは`../../target/transparency-probe/verified-run.log`。

画像証跡は `../../target/transparency-probe/evidence/`。
ログと画像は生成物としてGitの対象外。

**本体の縦書き、DirectWriteの文字タイル、分割表示、長文、4K、高DPI、複数モニター、
通常のタイトルバー・最大化・スナップ、メニューのポップアップは未検証。**
検証窓の操作欄は不透明であるが、本体の全メニューの動作を確認したものではない。
ウィンドウ全体のCPU描画と画像転送を使うため、本体での負荷を測る必要がある。
この方式の本体採用や、GPU合成方式への移行はまだ決定していない。

## 参照

- [Microsoft: UpdateLayeredWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-updatelayeredwindow)
- [Microsoft: Layered Windows with Direct2D](https://learn.microsoft.com/en-us/archive/msdn-magazine/2009/december/windows-with-c-layered-windows-with-direct2d)
- 元コード：Cargoキャッシュの `i-slint-backend-winit-1.17.1/renderer/sw.rs`。
  置換ファイルには元のライセンス表記を保持している。
