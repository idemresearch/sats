# sats launch videos

Short launch videos for sats releases, rendered with
[Remotion](https://www.remotion.dev) from terminal sessions recorded from
the real binary. Nothing on screen is mocked up: every terminal line is
output sats printed, replayed at a directed pace with a camera, highlights,
and captions around it.

The current video announces 0.0.4 and its Alkanes commands, in three shapes:
landscape (1920×1080), square (1080×1080), and vertical (1080×1920), at
60 fps.

## How it works

1. `scripts/capture.py` builds sats in debug mode and records each scene on
   a pseudo-terminal into `captures/` as [asciicast v2](https://docs.asciinema.org/manual/asciicast/v2/)
   files, which `asciinema play` can replay. It runs against the
   file-driven mock chain the integration tests use, in a disposable
   wallet restored from a BIP-39 test mnemonic and funded by the ignored
   test `crates/sats-cli/tests/video.rs`. The agent's side is a real MCP
   session with `sats agent serve`, saved as JSON. The only edits are
   redactions: the disposable home directory shows as `~` and the password
   as asterisks.
2. `src/demos/` holds one config per launch: the copy, which recordings
   to play, what is typed or pasted, and where the camera looks.
3. `src/lib/timeline.ts` turns a config and its recordings into one
   schedule: the terminal's byte stream, every keystroke, camera shots,
   highlights, and sounds. It checks that the typed text reproduces each
   recorded command exactly.
4. `scripts/sound.ts` synthesizes the soundtrack from that same schedule,
   so each key click lands on the frame its character appears. There is no
   licensed music or sample in it.
5. Remotion renders the frames (`src/`).

## Use

Requires Node 20.19+ and, to record, cargo and Python 3.

```sh
cd video
npm install
npm run dev        # Remotion Studio, to preview and scrub
npm run render     # every format to out/
npm run render -- landscape --concurrency=4
```

To refresh the recordings after a CLI change, run `npm run capture`, then
check the scenes in Studio: a config refers to lines by their text, so a
changed output may need a new highlight or shot.

Remotion downloads its own headless Chrome on first use. Where that isn't
possible, pass an installed one:
`npm run render -- --browser-executable=/path/to/chrome-headless-shell`.

## A new launch

Copy `src/demos/alkanes.ts`, add the recordings it needs to
`scripts/capture.py`, list it in `src/demos/index.ts`, and run
`npm run sound`. Shots, highlights, and captions are anchored to marks the
timeline emits, such as `send.output` (the first output line) or
`send.input.0` (the first answer to a prompt), plus a delay in seconds.

## License

Remotion is free for individuals and companies of up to three people;
larger companies need a [company license](https://www.remotion.pro/license).
Geist Mono is licensed under the SIL Open Font License.
