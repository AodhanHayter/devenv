# Render-pipeline benchmark fixtures

Drop raw PTY-master captures here as `*.bin`. The `render_pipeline` benchmark
picks up every `.bin` file automatically and replays it through the devenv shell
render pipeline (each runs with the status line off and on).

A capture is the raw byte stream a program writes to its terminal — exactly what
devenv's VT consumes when that program runs inside `devenv shell`. Capture the
program **directly** (not inside `devenv shell`, or you'd record devenv's
re-rendered output instead of the program's).

## Capture (macOS / BSD `script`)

Size your terminal to ~80×24 first so the recorded escape sequences match the
benchmark's geometry (the bench replays at 80×24):

```sh
# nvim: open a file, scroll/edit a bit, then :q
script -q benches/fixtures/nvim.bin nvim path/to/somefile

# claude-code TUI: interact briefly, then exit
script -q benches/fixtures/claude.bin claude
```

On Linux, `script` takes the command via `-c`:

```sh
script -q -c "nvim path/to/somefile" benches/fixtures/nvim.bin
```

`script` prepends a `Script started on …` header line to the file; it's a few
plain bytes and is harmless for the benchmark.

## Run

```sh
cargo bench -p devenv-shell --features bench-internals
```

Fixtures are git-ignored by default (they can be large and are machine-specific).
Commit one only if you want a shared baseline.
