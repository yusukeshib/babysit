# babysit

[English README](README.md)

ローカルコマンドに API を生やすツール。外部の AI エージェント（Claude
Code, Codex, …）がライブ出力と終了状態を、`gcloud` や `kubectl` を
クエリするのと同じ感覚でクエリできる。

**自分のシェル** — 普段通りに動かしたいコマンドをラップする。babysit が
セッション ID を表示し、コマンドはそのまま透過的に実行される:

```console
$ babysit -- make local-ci
babysit session ab12: make local-ci
  babysit log -s ab12 --tail 200
  babysit status -s ab12
Running tests...
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
```

**別ターミナルのエージェント** — セッション ID（`ab12`）を渡せば、必要な
タイミングで状態を引きにいける:

```console
$ babysit status -s ab12
session: ab12
cmd:     make local-ci
state:   exit:2
exit:    2

$ babysit log -s ab12 --tail 3
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
```

babysit 自身は監視を一切行わず、ラップされたコマンドを小さな
CLI / ファイル API として公開するだけ。いつ・どう使うかはエージェントが
決める。

## プロンプト例

セッション ID を渡したら、走り続けるコマンドを見ててくれる同僚に
頼むような感覚でプロンプトを書ける:

> `babysit` っていう CLI でセッション `ab12` を見ててほしい。
> `make local-ci` が終わったら教えて、コケてたらどのテストがどう
> 落ちたか要約して。

> `babysit` コマンドでセッション `ab12` を見張っといて。何かおかしく
> なった時だけ知らせて。

エージェントが自身のループで `babysit status` / `babysit log` を叩く
仕組みで、babysit 側からプッシュ通知を出すわけではない。

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

Nix（flakes）を使う場合:

```
nix run github:yusukeshib/babysit        # インストールせずに実行
nix profile install github:yusukeshib/babysit
```

インストール後は `babysit upgrade` で最新リリースに自己アップデート
できる。

## サブコマンド

```
$ babysit help
Wrap a shell command in a PTY and expose it to external agents via subcommands

Usage: babysit <COMMAND>

Commands:
  run      Wrap a shell command in a PTY and expose it via the other subcommands
  list     List all babysit sessions
  status   Show status of a session
  log      Show recent output from the wrapped command
  restart  Restart the wrapped command
  kill     Terminate the wrapped command
  send     Send text to the wrapped command's stdin (newline appended)
  prune    Delete sessions whose wrapped command has finished or whose owner died
  upgrade  Self-update to the latest version
  config   Print shell integration (completions)
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

各コマンドのフラグやエイリアスは `babysit help <command>` で見られる。
`babysit -- <cmd>` は `babysit run <cmd>` の短縮形。

セッション ID はデフォルトで自動採番されるが、`run` に `--id <id>` を
渡せば自分で指定できる（例: `babysit run --id ci -- make local-ci`）。
ID は一意で、使える文字は英数字・`-`・`_`・`.` のみ。

`-s <id>` は `--session <id>` の短縮形で、ID か `latest` という文字列を
受け付ける。ラップされたコマンドの内側からは `$BABYSIT_SESSION_ID`
経由でセッションが暗黙に決まるので、フラグは省略可能。

## デタッチモード

`babysit -d -- <cmd>`（または `babysit run -d -- <cmd>`）はコマンドを
バックグラウンドで起動して即座に戻り、デタッチされた babysit ワーカー
が監視を続ける:

```console
$ babysit -d --id ci -- make local-ci
babysit session ci: make local-ci
  babysit log -s ci --tail 200
  babysit status -s ci
$ # すぐにプロンプトが戻る。あとはエージェントが ci をポーリングする
```

ワーカーはこのシェルが終了しても生き残る。ライブ出力は端末には流れ
ない（端末が繋がっていないため）が、セッションログには記録されるので、
`babysit log`/`status`/`send`/`kill` はアタッチ時と同じように動く。

`status` と `log` は babysit 自身が終了した後でも動く（ディスク上の
状態ファイルにフォールバックする）。`restart`, `kill`, `send` はライブの
コントロールソケットが必要で、babysit プロセスが既にいない場合は失敗
する。

`babysit <unknown>` は未知のサブコマンドとして扱われ（`did you mean …?`
のヒント付き）、ラップ実行とはみなされない。ラップしたい場合は
`babysit -- <cmd>` か `babysit run <cmd>` を使うこと。

## シェル統合

`babysit config` を eval すると補完が有効になる:

```sh
# zsh (~/.zshrc)
eval "$(babysit config zsh)"

# bash (~/.bashrc)
eval "$(babysit config bash)"
```

サブコマンドとそのエイリアス、各コマンドのフラグ、そして最も便利な点として
`-s` のセッション ID 補完（`~/.babysit/sessions` を直接読む。`latest` も含む）
が効くようになる。`babysit run` はシェル本来のコマンド補完に委譲する。

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
