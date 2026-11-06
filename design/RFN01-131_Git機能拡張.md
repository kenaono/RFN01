# RFN01-131 Git機能拡張

2026-11-06 調査結果と設計。差分表示から部分Stageへ進む順序と、GitKrakenをGUIの品質目標にすることを会話で合意。リポジトリ一覧の具体仕様は設計案。[親Issue](https://linear.app/ken-aono/issue/RFN01-131)

## 目標

原稿やコードを編集している場所から、変更の確認、Commitの組み立て、履歴の調査、取り込みと衝突の解消、別ブランチでの並行作業を完結できるようにする。各作業は、どのリポジトリ・ブランチ・ファイルを対象にしているか、実行すると何が変わるかが画面で分かる状態を完了の基準にする。

GitKrakenを手本に、リポジトリを探す画面から、グラフ、変更ファイル、差分へ迷わず進めるGUIを目指す。各画面でリポジトリ名とブランチが見え、選んだ行と現在の作業先を区別できるようにする。実装ごとの画面確認では、キーボードで選択・実行・復帰できること、幅を狭めたりWindowsの表示倍率を上げたりしても対象と主要操作が見分けられることを確認する。[GitKrakenの画面構成](https://help.gitkraken.com/gitkraken-desktop/interface/)

## 現状と差分

ローカルのソースと `origin/main` のGit関連ファイルを照合した。現行仕様は [要件定義 8.7〜8.9](../要件定義.md) と [RFN01-67](https://linear.app/ken-aono/issue/RFN01-67) を参照。

| 画面 | 現行 | 拡張候補 |
| --- | --- | --- |
| Git Changes | ファイル単位のStage／Unstage、Commit／Amend、Stash、Fetch／Pull／Push／Sync | Stage済みと未Stageの差分を分け、変更ブロックを選んでStage／Unstage。衝突中は解決・続行・中止の入口を表示 |
| Git Repository | コミットグラフ、ローカルとリモートのブランチ、親／作業ツリーとの比較、Merge・Revert・Cherry-pick・Reset・操作履歴 | 読み込み範囲を越える履歴検索、ブランチ／コミット間の比較、Worktree一覧 |
| Git History | 現在の文書の履歴を名前変更前まで追い、過去版と編集中の本文を比較して差分を取り込む | 文書を調べる既存の入口として活用。行の由来を追うBlameは別候補 |

`git status` の結果はStage済みと未Stageを分けている。Git RepositoryのWIP差分を開くところで区分が落ち、どちらの行でもHEADとディスク上の作業ツリーを比べる。同じファイルをStageした後で別の箇所を編集すると、実際にCommitされる内容をWIP差分で確認できない。[git_ui.rs](../src/git_ui.rs)・[git_repo_ui.rs](../src/git_repo_ui.rs)

現行はGUIからのMerge・Pull・Revert・Cherry-pickの衝突を検知するとGitの `--abort` を呼ぶ。Stashには別の巻き戻し処理がある。Gitは未解決のファイルを返すが、画面に操作種別と続行手順を表す仕組みはない。衝突解消の追加はRFN01-67で合意した振る舞いを変えるため、独立して仕様を決める。[git.rs](../src/git.rs)

Git Changes上部の選択肢は、現在のWorkspaceに登録されたフォルダ。Git Repositoryもそこから選んだ一つを開くため、別のWorkspaceも含めてどのリポジトリを扱っているかを見渡す入口がない。Workspaceの登録台帳には、すべてのWorkspaceと登録フォルダ、その対応がある。[git_ui.rs](../src/git_ui.rs)・[workspace.rs](../src/workspace.rs)

## GUIクライアント調査

2026-11-06に公式資料を参照。各製品の操作画面は公式資料の記述と図を確認した。

| 製品 | 確認した操作 | RFN Editで参考にする点 |
| --- | --- | --- |
| Tower（Windows、有償） | 差分で変更ブロックまたは行を選んでStage。Reflogから過去の位置にブランチを作成 | 選択した範囲が分かる差分操作。履歴の復旧はまず失われたコミットを別ブランチに拾い直す導線を検討。[Stage](https://www.git-tower.com/help/guides/working-copy/stage-changes/windows)・[Reflog](https://www.git-tower.com/help/guides/commit-history/reflog/windows) |
| GitKraken Desktop（有償機能あり） | グラフのWIPからStage、Merge／Rebase、衝突編集、Worktreeの切り替え | グラフを作業の起点にし、変更・衝突・Worktreeのコンテキストを表示する。[Stage](https://help.gitkraken.com/gitkraken-desktop/staging/)・[Merge](https://support.gitkraken.com/gitkraken-desktop/branching-and-merging/)・[Worktrees](https://help.gitkraken.com/gitkraken-desktop/worktrees/) |
| GitHub Desktop（無償） | Commitに入れる変更の選択、履歴、Stash、Worktree管理 | 日常のCommit操作を見つけやすくし、並行作業も同じリポジトリから始められるようにする。[Commitとレビュー](https://docs.github.com/en/desktop/making-changes-in-a-branch/committing-and-reviewing-changes-to-your-project-in-github-desktop)・[ブランチでの作業](https://docs.github.com/en/desktop/making-changes-in-a-branch) |
| TortoiseGit（無償） | ログ検索、コミットの差分比較、Blameへの移動 | 履歴を探す、ファイルを絞る、変更箇所を読む、という流れを一つの画面からたどれるようにする。[Log Dialog](https://tortoisegit.org/docs/tortoisegit/tgit-dug-showlog.html) |

## データと実行

- Gitはユーザー環境の実行ファイルを `git::command` 経由で呼ぶ。資格情報、フック、設定は現行の契約を引き継ぐ。画面の差分表示とGitの書き込みを分け、実行、未保存文書の確認、完了後の更新は既存の `git_ui` の経路を使う。
- 差分が持つ比較元はHEADまたは指定のコミット、index、ディスク上の作業ツリー。未保存の本文は既存の「前回のCommitと比較」とGit Historyで参照する。新しい差分には両側の名前を表示し、ファイルが存在しない状態・空ファイル・バイナリ・読み取り失敗を区別する。
- 操作の対象は作業ツリーのルート、ファイルの相対パス、比較元、必要なコミットIDやブロックを組にして保持する。画面の行番号は選択の入口に使う。更新や確認ダイアログを挟んでも対象がすり替わらないよう、実行直前に照合する。
- 書き込みが失敗・中断した場合はGitの結果と最新状態を表示する。Gitが扱うパッチのバイト列を保ち、見せるために変換・折り返ししたテキストから適用データを組み立てない。Gitのロックや外部操作で対象が変わったときの再表示を試験する。
- ボタン・キー・パレットで共用する操作は既存のアクション表に接続する。ファイルやブロックをクリックしたときの対象選択は各画面で行い、書き込み前提の判定は共有する。[アクションの仕組み](アクションの仕組み.md)・[設定とアクションの一覧](設定とアクションの一覧.md)

## Sub Issueの作業単位案

### A. Stage済みと未Stageの差分を分ける

[RFN01-135](https://linear.app/ken-aono/issue/RFN01-135)

Staged ChangesではHEAD → index、Changesではindex → 作業ツリーを表示する。Git ChangesとGit RepositoryのWIPに「差分を表示」の入口を置き、同じ比較元を参照する。左ペインのダブルクリックは、現行どおりファイルを開いて編集中の本文を前回のCommitと比べる。未追跡ファイルは片側が無い差分、未CommitのリポジトリはHEAD側が無い差分として扱う。比較を左右に広げる場合にも表示名と内容を引き継ぐ。

完了条件は、同一ファイルにStage済みの編集Aと未Stageの編集Bがあり、各行を選ぶと片方だけが見えること。日本語・空白入りパス、追加・削除・名前変更、バイナリ、読み取り失敗を確認する。

### B. 変更ブロックのStage／Unstage

[RFN01-136](https://linear.app/ken-aono/issue/RFN01-136)

Aの比較元に従い、まず追跡済みテキストの変更ブロック（hunk）を一つずつ操作する。未Stageのブロックはindexへ適用し、Stage済みのブロックはindexへ逆向きに適用する。行単位は同じブロックに別の変更が混在する場合に必要になるため、部分パッチを組み立てる作業として切り分ける候補。

完了条件は、選択したブロックだけがStage／Unstageされ、ディスク上のファイルと選んでいない変更が保たれること。外部Gitや保存で差分が変わった場合は、対象を取り直す。未保存の本文があるときの案内、CRLF、Git属性・フィルタ、末尾改行の有無を確認する。適用前の照合とGitの実行をまたいで外部操作が競合する場合は実装時に検証する。[git-apply](https://git-scm.com/docs/git-apply)

### C. 履歴を検索し、任意の二点を比較する

[RFN01-137](https://linear.app/ken-aono/issue/RFN01-137)

Git Repositoryでコミットメッセージ、SHA、作者、パスから履歴を探し、既存の詳細・差分へ移る。読み込み済みのグラフだけで絞る操作と、リポジトリの履歴を検索する操作の違いを画面で示す。選んだ二つのコミットまたはブランチ先端の変更ファイルを表示する。比較開始時に両端のコミットIDを確定する。分岐点から片側への比較は、二点間比較とは別の選択肢として検討する。

完了条件は、初期200件の外にある変更を検索して該当の差分を開けること、指定した二点の比較で変更一覧と選んだファイルの差分がGitの結果と一致すること。

### D. Merge／Pullの衝突を解消して続行する

[RFN01-138](https://linear.app/ken-aono/issue/RFN01-138)

最初はMergeとMerge方式のPullを扱う。作業ツリーごとに進行中の操作と未解決パスをGitから読み、Terminalで始めたMergeも同じ画面に出す。共通祖先・両側の版と編集する結果を区別し、保存した結果を明示してStageする。未解決が無くなったらContinueを出す。Abortは対象と解決中の編集への影響を示して確認する。Gitの失敗と「衝突で一時停止」を分け、再起動時もGitの状態から復元する。

完了条件は、一時リポジトリの衝突を外部GUIなしで解消・続行でき、中止した場合はGitの中止結果が表示されること。未保存TAB、片側削除、外部からの解決、操作失敗時の再読込を確認する。Rebase、Cherry-pick、Revert、Stashの衝突は操作ごとの続行・中止の契約を検討してから広げる。

### E. Worktreeの一覧・作成・表示切り替え

[RFN01-139](https://linear.app/ken-aono/issue/RFN01-139)

Git RepositoryからWorktreeのパス・ブランチ・現在位置を読み、作成済みのWorktreeをRFN Editで開く。共通Gitディレクトリで同じリポジトリのWorktreeを束ね、選択・状態・index・未保存確認は作業ツリーのルートで区別する。作成先の選択とWorkspaceへの登録方法を先に決める。削除・移動・ロックのUIはその後に検討する。

完了条件は、同じリポジトリの二つのWorktreeを行き来し、一方のStage・Commitと未保存確認がもう一方に誤って適用されないこと。[git-worktree](https://git-scm.com/docs/git-worktree)

### F. 管理しているリポジトリの一覧

[RFN01-140](https://linear.app/ken-aono/issue/RFN01-140)

GitKrakenのRepository Managementを参考に、Git Changesの見出しとFileメニューから同じ「Repositories…」を開けるようにする案。最初にすべての登録リポジトリを表示し、現在のWorkspaceで絞れるようにする。名前・ブランチ・所属Workspaceを行に出し、同名のリポジトリはローカルパスで見分けられるようにする。現在の対象は選択行と別の印で示す。[GitKrakenのリポジトリ管理](https://help.gitkraken.com/gitkraken-desktop/open-clone-init/)

| 一覧の例 | ブランチ | Workspace |
| --- | --- | --- |
| ● RFN01　`D:\Dev\RFN01` | main | 開発、検証 |
| Manuscript　`D:\Writing\Manuscript` | draft | 執筆 |
| RFN01　`D:\Sandbox\RFN01` | feature/ui | 試作 |

Workspaceの登録台帳から、各登録フォルダのGitルートを解決する。同じ作業ツリーは一行にまとめ、所属先を併記する。別パスのCloneは別行として扱う。Linked Worktreeは操作対象を作業ツリーごとに識別し、Eで同じリポジトリの配下にまとめる表示を検討する。Gitに管理されていないフォルダと、パスや権限の問題で確認できない登録フォルダを区別する。一覧の読み取りは裏で行い、Git Repositoryのグラフは選択先を開いた時点で読む。

現在のWorkspaceに属する行はGit Changesの対象を選び、既存のGit Repositoryを開く。別のWorkspaceに属する行では、開くWorkspaceを明示して既存の切り替え手順を通す。所属先がない登録フォルダは、その状態を表示してWorkspace管理への入口を出す案。既存のClone／初期化の流れを使い、登録やGitの初期化の結果は一覧に反映する。

完了条件は、複数のWorkspaceのリポジトリを一覧でき、同じ作業ツリーの重複と別のCloneを区別できること。選んだ行のパスがGit ChangesとGit Repositoryに反映され、Workspaceをまたぐ場合も切り替え先が事前に分かること。Workspaceが開いていない状態、Git未導入、アクセス不可、キーボードでの選択も確認する。

## 順序と未決事項

- A → Bの順で進める。初回はAの差分表示まで。Bは変更ブロック単位から始める。CとFはこの二つから独立した作業単位。
- DとEの順は、衝突解消と並行作業のどちらを先に日常で使いたいかで決める。Dは現行の自動中止を変えるため、継続する操作の範囲と初期動作を合意する。Eは既存のWorkspaceとの関係を決める。
- タグ、リモート・上流の編集、Stashの差分、Blame、Reflog、Rebase（対話型を含む）、force-with-lease、Submodule／LFS、ホスティングサービスのPR連携は後続候補。実際に外部クライアントを使う場面を確認し、必要な作業を追加する。

## 技術確認

WindowsのGit 2.54.0で一時リポジトリを作り、次の三点を確認した。

1. 日本語名の同一ファイルでStage済み・未Stageの差分を別々に取得し、`git apply --cached` と逆適用でindexだけが変わった。
2. Linked Worktreeは共通Gitディレクトリを共有し、異なるindexを使った。`git worktree list --porcelain -z` で両方のパスを取得した。
3. Merge衝突時に `MERGE_HEAD` とporcelain v2の未解決レコード、indexの三つの版を取得した。試験のクリーンな作業ツリーは `git merge --abort` で元に戻った。

確認範囲はGit CLIの挙動。RFN Editの実画面での検証は各実装Issueで行う。
