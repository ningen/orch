# orch

ローカルでコーディングエージェントを束ねる作業台。1タスク = 1つの git worktree + 1つの
エージェントセッションで、ブラウザの UI から複数のタスクを並行して走らせる。

```
$ orch --repo ~/ghq/github.com/ningen/dotfiles
orch 0.1.0
  repo   /home/ningen/ghq/github.com/ningen/dotfiles
  branch main
  url    http://127.0.0.1:7717
```

![orch](docs/ui.png)

## なぜ作ったか

AI コーディングのボトルネックは、もうコードを書くことではなく「複数のエージェントが
触ったものをどう検証して main に積むか」にある。既存の GUI は worktree 分離と diff
レビューまでで止まっていて、オーケストレーション層が空いている。

`orch` はその層に寄せる。

- **エージェント本体は作らない**。ACP (Agent Client Protocol) を話すものをそのまま使う
- **1タスク1 worktree**。並行して走らせても作業ツリーが衝突しない
- **会話・ツール呼び出し・usage を SSE で流す**。UI は状態を持たない薄いクライアント
- **検証は外側のコマンド**。`nix build` でも `cargo test` でも、決定論的なゲートだけを信じる

## 動かす

```bash
cargo run -- --repo /path/to/repo
# 既定だと http://127.0.0.1:7717 が開く
```

既定のエージェントは `opencode acp`。差し替えは `--agent`:

```bash
orch --agent "npx @agentclientprotocol/claude-agent-acp"
orch --agent "codex-acp"
orch --agent "gemini --acp"
```

| フラグ | 意味 |
| --- | --- |
| `--repo PATH` | 作業対象の git リポジトリ（既定: カレント） |
| `--port N` | 待ち受けポート（既定: 7717） |
| `--agent CMD` | 起動する ACP エージェント |
| `--no-browser` | ブラウザを開かない |

状態は `$ORCH_HOME`（既定 `~/.orch`）に置く。worktree とタスクの JSON がここに入る。

## 構成

```
ui/            ブラウザ UI（ビルド不要、バイナリに埋め込み）
src/acp.rs     ACP クライアント（子プロセスと JSON-RPC over stdio）
src/task.rs    タスク = worktree + セッション + イベント配信 + 永続化
src/server.rs  axum: JSON API / SSE / 埋め込みアセット
src/main.rs    CLI
```

エージェントはこんな形で喋る。

```
[ブラウザ] --HTTP+SSE--> [orch daemon] --ACP/stdio--> [opencode acp]
                             |
                             +-- git worktree (タスクごと)
                             +-- ~/.orch/tasks/<id>.json (会話の永続化)
```

`session/update` の `agent_message_chunk` / `tool_call` / `usage_update` をそのまま
イベントに変換して UI に流す。モデル切替は `session/set_config_option` をそのまま使う。

## いまできること

- タスク作成 → worktree を切る → エージェント起動 → ストリーミング表示
- マルチターン（同じセッションに追加プロンプト）
- タスクごとのモデル切替（エージェントが提示した選択肢から）
- ツール呼び出しの可視化（write / bash / read など、入力と出力つき）
- 変更 diff の表示（分岐元コミットとの比較）
- 任意コマンドの実行と出力ストリーミング（`nix build` など）
- トークン数とコストの表示
- タスクのコミット（マージ用のコマンドを返す）

## これから

- [ ] 権限リクエストを UI で承認する（今は自動承認）
- [ ] タスク間の依存 DAG（A の成果を B が使う）
- [ ] 検証ゲートを Nix derivation にする（ローカルと CI で同じ定義）
- [ ] マージキュー（検証を通ったものだけ直列に積む）
- [ ] セッションの復元（`session/load`）
- [ ] GPUI / Tauri のネイティブシェル

## 設計メモ

- **なぜ ACP か**: Claude Code / Codex / Gemini / OpenCode / DSH が全部 ACP を話す。
  クライアントを1回書けば harness を差し替えられる。CLI のフラグ変更に振り回されない。
- **なぜ worktree か**: 並行実行の唯一まともな分離。コンテナより軽く、diff がそのまま取れる。
- **なぜ状態を JSON に落とすか**: デーモンを再起動しても会話が消えない。エージェント側の
  セッションは切れるので、そこは `session/load` で戻す予定。
