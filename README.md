# PhotoDrop

Send photos and videos from an iPhone to a Linux laptop over Wi-Fi. A small Rust (axum + tokio) server serves one upload page, and Safari on the phone uploads to it. No iOS app needed.

Status: work in progress.

## What works now

- `GET /` serves the upload page; you can select several files.
- The page uploads files one at a time (one `POST /upload` per file) and shows one status line per file.
- Uploads are streamed to `./inbox/` with no size limit.
- The original file name is kept. A repeated name gets ` (1)`, ` (2)`, … before the extension, and existing files are never overwritten. Missing or invalid names become `upload.bin`.

## Known limitations

- **No access control yet.** The server listens on `0.0.0.0:8000`, so anyone on the same network can upload. Run it only while you're using it, and never expose it to the internet. A token is planned for M4.
- **Photos may not arrive as originals.** In tests with the Photo Library picker, Safari converted HEIC photos to JPEG and renamed them `.jpeg`.
- An interrupted upload can leave a partial file in `inbox/` (M3 plans to write `.part` files first).
- Keep the iPhone screen on during large uploads; Safari stops uploading in the background.
- Port and folder are fixed for now (`8000`, `./inbox`). CLI flags are planned for M4.

## Run

With Cargo:

```sh
cargo run --release
```

The server creates `inbox/` if it doesn't exist.

With Docker:

```sh
docker build -t photodrop .
mkdir -p inbox   # create it yourself so it isn't owned by root
docker run -d --rm --name photodrop -p 8000:8000 \
  -v "$PWD/inbox:/data/inbox" --user "$(id -u):$(id -g)" photodrop
```

If host port 8000 is taken, map another one, e.g. `-p 8080:8000`.

On the iPhone, open `http://<laptop-ip>:8000/` in Safari. If the laptop runs avahi, `http://<hostname>.local:8000/` also works.

## Develop

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
