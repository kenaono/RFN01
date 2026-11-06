# RFN Edit のテスト

## WSL で共通ロジックを確認する

UbuntuのWSLにRustとCargoを入れ、リポジトリのルートから実行する。初回はCargoが使えることを `cargo --version` で確認する。

```sh
cargo test --locked --offline --manifest-path Test/wsl/Cargo.toml --target-dir target/wsl-tests
```

この入口はアプリ本体と同じRustソースを読み、行・文字の差分、Gitのグラフ、キルリング、ペインの配置、本文のブロック解析、Workspaceのパス境界を検証する。WSLではLinuxのシンボリックリンクを使った境界テストも走る。追加のCargo依存はない。

## Windows でアプリ全体の Cargo テストを実行する

`cargo test --locked --offline` を基本とする。並列度はリポジトリ設定の4本を使う。

| 対象 | 主な確認 |
| --- | --- |
| 本文・ファイル | 編集と取り消し、文字コードと改行、保存・復旧、Workspaceの制約、パスの境界 |
| 画面の状態 | オフスクリーンのSlintで、行やボタンの入力、フォーカス、ダイアログ、操作対象の引き継ぎを確認 |
| 組版・印刷 | 改ページ、書字方向、描画データを確認 |
| Git・Terminal | 一時リポジトリでの操作、端末の入力・表示・状態を確認 |

毎回実行するテストには、一時ファイルやオフスクリーンで安定して判定できるものを置く。Explorerの右クリックメニューと更新抑止のテストもここに含める。

`cargo test --locked --offline -- --list` で一覧を、`cargo test --locked --offline -- --ignored --list` で通常は除外する一覧を確認できる。描画スナップショットや計測、本物のシェル・WSL・印刷を使うケースは、目的に合うテスト名を指定して個別に実行する。アプリからWSLを起動する試験はWindows側で実行する。DirectWrite、WindowsのCP932変換、ConPTY、ジャンクションなどもWindows側で確認する。印刷ジョブを実際に出すテストは、利用する出力先を確認してから実行する。

## Windows の実画面で確認すること

- [Windows実画面のシナリオ](RFN01-116_Windows実画面.md)で、Windowsのファイルダイアログ、IME、印刷ダイアログとの境界を確認する。
- [ファイルツリーのフォーカス](RFN01-115_ファイルツリーのフォーカス.md)で、連続したキー操作と確認画面からの復帰を確認する。

結果はシナリオ名、対象の実行ファイル、環境（Windows・IME・プリンターのうち関係するもの）、結果（成功／失敗／未実施）、失敗した操作を記録する。期待結果まで観察したケースを成功とする。
