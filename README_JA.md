# babysit

[English README](README.md)

ローカルコマンドに API を生やすツール。外部の AI エージェント（Claude
Code, Codex, …）がライブ出力と終了状態を、`gcloud` や `kubectl` を
クエリするのと同じ感覚でクエリできる。

```console
$ babysit -- make local-ci
babysit session ab12: make local-ci
  babysit log -s ab12 --tail 200
  babysit status -s ab12
Running tests...
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
$ echo $?
2
```

別ターミナルのエージェントに、表示されたセッション ID をそのまま渡す:

> *「babysit セッション `ab12` で何かおかしくなってないか見てくれる？」*

エージェントは `babysit log` / `babysit status` で状態を読みに行く。
babysit 自身は監視を一切行わず、ラップされたコマンドを小さな
CLI / ファイル API として公開するだけ。いつ・どう使うかはエージェントが
決める。

## なぜ作ったか

リモート実行系のプラットフォーム（`gcloud`, `kubectl`, CI など）には、
AI エージェントがログや状態を取得するための API が用意されている。
一方ローカル実行にはそれがない。ターミナルで走っているコマンドは、その
TTY に既に貼り付いているエージェント以外からはブラックボックスで、実行
中の処理を解析しようと思うと結局スクロールバックを手でコピペすること
になる。

babysit はそのギャップを埋める。ローカルのコマンドを一度ラップすれば、
ライブの出力と終了状態が、エージェントが既に扱い慣れている小さな CLI
からクエリ可能になる。スクレイピングも画面共有も追加の常駐プロセスも
いらない。

## インストール

```
curl -fsSL https://raw.githubusercontent.com/yusukeshib/babysit/main/install.sh | sh
```

チェックサム検証つきでバイナリを `~/.local/bin/babysit` に置く
（`BABYSIT_INSTALL_DIR` で配置先、`BABYSIT_VERSION=v0.2.4` で
バージョンを上書き可能）。macOS / Linux の x86_64 / aarch64 対応。

または [GitHub Releases](https://github.com/yusukeshib/babysit/releases)
からビルド済みバイナリを直接取ってくるか、ソースからインストールする:

```
cargo install --git https://github.com/yusukeshib/babysit
```

インストール後は `babysit upgrade` で最新リリースに自己アップデート
できる。

## サブコマンド

```
babysit -- <cmd> [args…]                    # コマンドをラップ（短縮形）
babysit run [--name NAME] <cmd> [args…]     # コマンドをラップ（名前付き形）
babysit list [--json]                       # 全セッション           (alias: ls)
babysit status -s <id> [--json]             # ラップ中コマンドの状態 (aliases: st, info)
babysit log -s <id> [--tail N] [--raw]      # 出力（ANSI 除去済み）  (alias: logs)
babysit restart -s <id>                     # kill + 再起動          (alias: r)
babysit kill -s <id>                        # 終了させる             (alias: stop)
babysit send -s <id> "<text>"               # テキスト + 改行を送る  (alias: type)
babysit prune [--dry-run]                   # 終了済み / dead セッションを削除
babysit upgrade                             # 最新リリースに自己アップデート
```

`-s <id>` は `--session <id>` の短縮形で、ID か `--name` で付けた名前、
または `latest` という文字列を受け付ける。ラップされたコマンドの内側
からは `$BABYSIT_SESSION_ID` 経由でセッションが暗黙に決まるので、
フラグは省略可能。

`status` と `log` は babysit 自身が終了した後でも動く（ディスク上の
状態ファイルにフォールバックする）。`restart`, `kill`, `send` はライブの
コントロールソケットが必要で、babysit プロセスが既にいない場合は失敗
する。

`babysit <unknown>` は未知のサブコマンドとして扱われ（`did you mean …?`
のヒント付き）、ラップ実行とはみなされない。ラップしたい場合は
`babysit -- <cmd>` か `babysit run <cmd>` を使うこと。

## ディスク上のセッション状態

各セッションは `~/.babysit/sessions/<id>/` に書き出される:

```
meta.json       # 静的な情報（cmd, started_at, …）
status.json     # ライブの状態（running / exited / killed, exit_code）
output.log      # ラップ対象コマンドの生の出力
control.sock    # サブコマンドが通信する Unix ソケット
```

`babysit list` は、所有していた babysit プロセスが死んでいるセッション
（クラッシュ、kill -9、クリーンに終了状態を書き出す前のリブートなど）を
`dead` としてマークする。動いていないものを掃除するには
`babysit prune` を実行する。

## ソースからビルド

```
cargo build --release
# バイナリは target/release/babysit
```
