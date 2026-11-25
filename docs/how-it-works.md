# How it works

`x-stitch` never talks to X's own API. It asks a public metadata service
which photos a post has, downloads them from X's image CDN, and joins them.
Everything lives in `src/main.rs`.

| Step | Function | Role |
| --- | --- | --- |
| Parse | `parse_tweet_url` | Turns a pasted link into a user and a numeric post id |
| Resolve | `process_tweet` | Asks `api.fxtwitter.com` for the post's author and photo URLs |
| Download | `download_images` | Fetches and decodes every photo, one thread each |
| Render | `render`, `stitch_images` | Passes a lone JPEG through, or stitches and encodes |
| Tag | `insert_jpeg_comment` | Writes the post URL into a JPEG `COM` segment |

## A run

1. **Parse.** The scheme, query string and fragment are dropped. The host must
   be one of the X, Twitter or fxtwitter-style domains, and the path must be
   `/<user>/status/<id>` or `/i/web/status/<id>` with a numeric id. Anything
   after the id, such as `/photo/1`, is ignored.
2. **Resolve.** One request to `https://api.fxtwitter.com/<user>/status/<id>`
   returns the author's screen name and the photo list. Videos and GIFs are
   ignored; a post with no photos is an error. The screen name must match
   `[A-Za-z0-9_]`, because it ends up in the output path.
3. **Pick the original size.** Each photo URL gets `name=orig`, replacing any
   other `name` value, so X serves the largest stored version.
4. **Download.** Every photo is fetched and decoded on its own thread over a
   shared HTTP client. Order follows the post, not arrival.
5. **Render.** See [Rendering](#rendering).
6. **Tag and save.** The canonical URL `https://x.com/<screen_name>/status/<id>`
   is inserted as a `COM` segment unless `--no-metadata` is set. The output
   directory is created at this point, so a failed run leaves nothing behind.
   An existing file of the same name is replaced.

With several URLs, posts are processed one after another. A failure is
printed to `stderr` and the rest still run; the exit status is non-zero if
any failed.

## Rendering

| Input | Output |
| --- | --- |
| One JPEG | The downloaded bytes, unchanged |
| One photo in another format | Re-encoded as JPEG |
| Several photos | Stitched, then encoded as JPEG |

A lone JPEG is passed through because re-encoding it can only lose quality
and grow the file. `--quality` has no effect in that case.

Stitching lays segments left to right, or top to bottom with `--vertical`:

- The canvas is the sum of the segments along the stitch axis and the
  largest segment across it.
- Segments are aligned to the top, or to the left with `--vertical`.
- If sizes differ across the axis, the gap stays black and a warning is
  printed to `stderr`.
- Segments are converted to 8-bit RGB first, so transparency is dropped.

The stitched JPEG uses `--quality` (default 95) without chroma subsampling.
It is full resolution, but it is a new encode: not byte-identical to what X
serves, and it can be larger than the segments combined.

## The source comment

`COM` is a standard JPEG segment (marker `0xFF 0xFE`) that image viewers
ignore and metadata tools show. It is inserted right after the JFIF `APP0`
segment if the file starts with one, otherwise right after the start-of-image
marker. No pixel data is touched, which is what lets a passed-through JPEG
stay otherwise identical to the original.

## Limits

| Limit | Value | What happens beyond it |
| --- | --- | --- |
| Connect timeout | 10 s | The post fails |
| Read and write timeout | 30 s per socket operation | The post fails; a slow download is fine while data keeps arriving |
| Download size | 100 MB per photo | The post fails |
| Output size | 65535 px per side, the JPEG maximum | Encoding fails after the downloads |

## What it depends on

`api.fxtwitter.com` is a third-party service, and the only source of post
metadata. If it is down or changes its response format, `x-stitch` stops
working until it is back or the code is updated. Posts the service cannot
see, such as deleted ones, cannot be resolved.
