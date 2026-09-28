# 2026-09-17 レビューの再現検証

**修正後：** R1〜R4の回帰テストは`src/recovery_ui_tests.rs`に組み込み済みです。通常の`cargo test --offline --quiet`で実行されます。以下のファイルは修正前の再現記録であり、現在のソースへ再追加する必要はありません。

`reproduction-tests.rs`は、現在の`src/memo_ui_tests.rs`の末尾に一時的に追加して実行した3件のテストです。同ファイルの`use super::*`と`Offscreen`、アプリの内部関数を使用するため、単独のRustプログラムではありません。

実行コマンド：

```powershell
cargo test --offline --quiet memo_ui_tests::review_ -- --test-threads=1
```

3件とも、望ましい動作をassertし、現在の実装では失敗しました。出力は`reproduction-results.txt`にあります。通常の既存テストの失敗とは区別してください。

- `review_undo_must_retire_stale_backup`：保存内容までUndoしたあと、復元すると取り消したXが戻る。
- `review_restored_unsaved_text_must_stay_dirty_after_undo`：復元した未保存本文へ追記してUndoすると保存済み扱いになる。
- `review_missing_origins_must_have_distinct_recovery_ids`：別々の参照先を失った2件の復元後の退避先が同じになる。

既存テストの隔離用初期化を流用し、`app_data::TEST_DIRECTORY`で一時フォルダを指定しています。実行後、`src/memo_ui_tests.rs`は変更前のバイト列に戻しました。この記録は通常のテスト対象には追加されていません。
