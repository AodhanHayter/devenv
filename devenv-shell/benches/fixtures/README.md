# Render-pipeline benchmark fixtures

Drop raw PTY-master captures here as `*.bin`. The `render_pipeline` benchmark
replays each one through the devenv shell render pipeline (status line off and
on, legacy vs optimized).

A capture is the raw byte stream a program writes to its terminal — exactly what
devenv's VT consumes when that program runs inside `devenv shell`. Capture the
program **directly** (not inside `devenv shell`, or you'd record devenv's
re-rendered output instead of the program's).

## Geometry matters

A capture's escape sequences assume the terminal size it was recorded at. Tag the
filename with that geometry: `<name>.<cols>x<rows>.bin` (e.g. `nvim.200x54.bin`).
The bench only loads a fixture whose tag matches the run geometry. Untagged
`<name>.bin` files load at any size (use only if you know it's size-agnostic).

Set the bench geometry with `DEVENV_BENCH_COLS` / `DEVENV_BENCH_ROWS` (default
80×24).

## Capture at a specific size (tmux `pipe-pane`)

`pipe-pane` records the raw program output from a detached, exact-sized pane —
no real window needed:

```sh
COLS=200 ROWS=54
tmux -L cap new-session -d -x $COLS -y $ROWS -s s
tmux -L cap pipe-pane -t s -o "cat >> nvim.${COLS}x${ROWS}.bin"
tmux -L cap send-keys -t s "nvim path/to/file" Enter
# ... drive it with more send-keys (scroll/edit), then :q! ...
tmux -L cap kill-server
```

Trim the leading shell-prompt echo / trailing prompt so the fixture is the pure
program stream (start `ESC[?1049h`, end `ESC[?1049l`).

`script` also works (`script -q nvim.bin nvim file`), but tmux lets you pin the
exact geometry.

## Run

```sh
cargo bench -p devenv-shell --features bench-internals                    # 80x24
DEVENV_BENCH_COLS=200 DEVENV_BENCH_ROWS=54 cargo bench -p devenv-shell --features bench-internals
```

Fixtures are git-ignored (large, machine-specific). Commit one only for a shared
baseline.
