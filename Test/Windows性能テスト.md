# Windows 性能テスト

Windows の Release ビルドで計測する。通常の Cargo テストは機能の回帰を確認し、この手順は処理時間を比較する。比較時は同じ PC、ビルド設定、ウィンドウ寸法、倍率、表示モードで実行し、他のビルドや負荷の高い処理を止めてから測る。

## 自動計測

Windows のリポジトリのルートから PowerShell 7 で実行する。

```powershell
pwsh -NoProfile -File Test/Invoke-WindowsPerformance.ps1
```

固定のデータを生成して Release のテストを1本ずつ実行する。結果は `target/editor-perf/results/windows-*.log` に残る。各 `perf case=...` 行に測定数、中央値、p95、最大値が出る。機能検証に失敗した場合は失敗として扱う。実行時間は PC や実行条件によって変わるので、固定の合否閾値には使わない。

| ケース | データと操作 | 計測区間 |
| --- | --- | --- |
| `editor_many_paragraphs` | 約2 MiBの Markdown。短い段落が8,000以上あり、先頭の目印付近で1文字ずつ入力する | 入力処理の復帰、可視範囲の組版・タイル更新、残りの組版が反映されるまで |
| `editor_long_paragraph` | 約200 KiBの Markdown。先頭の単一段落は約24 KiBで、段落内の目印付近で1文字ずつ入力する | 同上 |
| `terminal_plain_band` | 120列・8行。通常の文字の1帯を描く | DirectWrite／Direct2D の描画とビットマップからのピクセル取り出し |
| `terminal_mixed_band` | 120列・8行。ANSI 色、太字、全角文字、カーソルを含む1帯を描く | 同上 |
| `terminal_mixed_viewport` | 120列・40行。同じ種類の文字を含む5帯を描き直す | 同上 |

自動計測では、実際の画面提示やIME変換を待つ時間は測らない。Terminal の自動計測は入力済みのセルを描くため、PTY通信やANSI解析も含まない。

## 改修前後の比較

1. 改修前と改修後のコミットで同じ自動計測を実行し、両方のログを残す。ログ先頭の `revision`、作業ツリーの状態、フィクスチャの SHA-256 で計測対象を照合する。
2. 同じ `case` と `metric` の中央値と p95 を比べる。数値が悪化した場合は両方のコミットで繰り返し測定し、ばらつきの範囲か再現する劣化かを確認する。
3. 再現する差は、対象の操作、計測条件、改修前後の値を変更の検証結果に記録する。実画面の見え方や操作感を比べるときは、下の同じシナリオを両方で実行する。

## 実画面での共通準備

Computer Use で対象ウィンドウを特定し、画面が変わるたびに観察する。次のコマンドはシェルの実行ツールで実行する。

```powershell
cargo build --release --locked --offline
pwsh -NoProfile -File Test/Start-WindowsPerformanceScenario.ps1 -Case EditorLarge
```

起動スクリプトは `EditorLarge`、`EditorLong`、`Terminal` のいずれかを受け付ける。比較する別の Release は `-Executable` に絶対パスを指定する。ケースごとに独立した実行ファイル、テスト用 `LOCALAPPDATA`、文書のコピーを `target/editor-perf/ui/` に作り、プロセスID、実行ファイル、診断ログの場所を出力する。Computer Use では表示名だけで判断せず、返されたアプリのパスが起動スクリプトの `Executable` と一致するウィンドウを1件選ぶ。倍率は120%に設定済み。ウィンドウ寸法は比較する実行間で揃え、記録する。

### P01 大きな Markdown ファイル

1. `-Case EditorLarge` で起動し、`many-paragraphs.md` の見出しと `RFN_PERF_EDIT_HERE` が見えることを確認する。
2. 見出しの先頭に `#` が見えていれば Ctrl+Shift+V で横書きプレビューに切り替える。目印の直後に本文カーソルを置き、フォーカスを確認してから文字キー `a` を20回押す。本文に20文字入り、カーソルが追従することを確認する。文字列の一括貼り付けと区別する。
3. 本文内を下へ3回、上へ3回スクロールする。Computer Use では1回の `scrollY` を下へ600、上へ-600に揃える。位置が変わり、本文が描き直されることを確認する。
4. Ctrl+End で末尾を表示して1文字入力し、Ctrl+Home で先頭に戻る。先頭と末尾が表示でき、操作が滞らないか記録する。

### P02 長い段落

1. `-Case EditorLong` で起動し、`long-paragraph.md` と `RFN_PERF_EDIT_HERE` を確認する。
2. P01 と同じように横書きプレビューにし、目印の直後にカーソルを置いて `a` を20回押す。入力した文字と長い段落の行が欠けず、スクロール位置とカーソルが追従することを確認する。
3. 本文内を下へ3回、上へ3回スクロールする。長い段落の途中で本文が抜けたり重なったりしないことを確認する。

Editor のケースでは生成したコピーを Ctrl+S で保存してから、テストのウィンドウを閉じる。比較のたびに起動スクリプトで新しいコピーを用意する。

### P03 Terminal の描画

1. `-Case Terminal` で起動する。隔離したセッションから `RFN Perf Fixture` のTABが開き、固定データが出力される。Computer Use ではこのTABを観察する。
2. 色付きの `20_World`、`rfn-bench.sh`、全角の `日本語の描画` が表示され、最後に `RFN-PERF-TERMINAL END rows=640` が見えることを確認する。ファイル名が途中で次の行に重ならないことも確認する。
3. 履歴を上へ3回、下へ3回スクロールする。行番号がつながり、色や文字の欠けがないことを確認する。
4. 終了した Terminal TAB を閉じてから、テストのウィンドウを閉じる。

### 記録

| 対象 | 記録するもの |
| --- | --- |
| 共通 | シナリオ名、ビルド、PC、ウィンドウ寸法、倍率、成功・失敗・未実施、観察された症状 |
| Editor | `Perf_*.log` の `session` が `profile=release` であること、`edit` の件数・`total`、`Diag_*.log` の `scroll.p0` と新規タイル数 |
| Terminal | `Diag_*.log` の `terminal frames` の `drawn`、`draw`、`bands` と終了マーカー。描画時間は区間の合計なので、単一フレームの時間として読まない |

画面キャプチャや Computer Use の操作の間隔は入力遅延の計測値に使わない。ログに画面提示の計測がない場合は、実際の打鍵から表示までの遅延を未計測と記録する。
