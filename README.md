# HTTP Request Client (a Zed extension)

An extension for [Zed](https://zed.dev) that adds support for the `.http` (and
`.rest`) language — in the same spirit as VS Code's REST Client — letting you
write HTTP requests in plain text files, with syntax highlighting and request
execution straight from the editor.

Example file used for development/testing: [`api.http`](./api.http), with
variables read from a local `.env` — copy the template before using it:

```sh
cp .env.example .env
```

The `.env` is looked up starting from the folder of the `.http` file itself and
walking up to the workspace root, so you can keep one `.env` per environment
(e.g. `.rest/prd/.env`, `.rest/local/.env`). The one closest to the file wins.
The `.env` itself is not versioned (see [`.gitignore`](./.gitignore)).

## Current status

- [x] **Syntax highlighting** for `.http`/`.rest`
- [x] Native language server in Rust (real request execution)
- [x] `▶ Send request` Code Lens above every HTTP method
- [x] Loading indicator in place of the Code Lens while the request runs
- [x] Result display (status line + headers + formatted body) in a tab on the
      side
- [x] Language server binary resolution (Zed settings → local build of the
      repository → `$PATH` → release download)
- [x] Distribution: downloads the binary from the GitHub Release automatically,
      with no need for `cargo install` (see `src/lib.rs`)

### Syntax highlighting

Implemented declaratively, using the
[tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http)
grammar (MIT), the same one used by `rest.nvim`. It recognizes the structure of
a `.http` file: method, URL, HTTP version, headers, body, comments, request
separators (`###`), variable declarations (`@NAME = value`) and interpolations
(`{{NAME}}`).

What gets colored:

| Element                                     | Example in `api.http`                         |
|---------------------------------------------|-----------------------------------------------|
| HTTP method                                 | `POST`, `GET`                                 |
| URL                                          | `{{HOST}}/v1/oauth/login`                     |
| Header name                                  | `content-type`, `Authorization`               |
| Variable interpolation                       | `{{USERNAME}}`, `{{oauthLogin.response...}}`  |
| Variable declaration                         | `@HOST = ...`                                 |
| Comment metadata (`# @name`)                 | `# @name oauthLogin`                          |
| Request separator                            | `###`                                         |
| HTTP status/version (in pasted responses)    | `HTTP/1.1`, `200`, `OK`                       |
| JSON/XML body                                | injected with Zed's native JSON/XML grammar   |
| Commented-out query param                    | `    # &sort=asc`                             |

`json`/`xml` bodies are highlighted recursively via *injection*, using the JSON
and XML grammars already bundled with Zed — the same mechanism that colors code
blocks inside Markdown.

The grammar in use is a **fork** of upstream, in
[`grammars-src/`](./grammars-src/README.md), with two patches:

- a commented-out line inside a multi-line query string was swallowed by the URL
  and ended up with the URL color, indistinguishable from an active parameter;
  it now becomes a `(comment)` node;
- indentation with **TAB** was not recognized as whitespace (TAB is `\p{Cc}`,
  and the grammar used `\p{Zs}`), which made every line of a multi-line query
  string become a standalone request. That affected the whole file, not just
  query strings.

## Project layout

A Cargo workspace with two crates: the WASM extension (which Zed loads, and
which only starts the language server) and the native language server (which
does all the work — parsing, variable resolution and the HTTP requests).

```
.
├── extension.toml              # extension manifest, grammar and language server
├── Cargo.toml                  # workspace: extension crate (cdylib) + lsp-server
├── src/
│   └── lib.rs                  # WASM extension: only locates/starts the lsp-server
├── lsp-server/
│   └── src/main.rs             # language server: .http parser, variables,
│                               # HTTP execution (ureq), code lens and commands
├── languages/
│   └── http/
│       ├── config.toml         # association of .http/.rest with the language
│       ├── highlights.scm      # syntax highlighting rules
│       └── injections.scm      # JSON/XML injection inside the body
├── grammars-src/
│   └── tree-sitter-http/       # grammar fork (its own git repo); this is what
│                               # extension.toml references. Not to be confused
│                               # with grammars/, the checkout Zed generates
├── api.http                    # example/documentation file
├── example.csv                 # used by the upload example (`< ./example.csv`)
└── .env.example                # template for the variables used by api.http
```

## Testing locally in Zed

### Prerequisites

**Rust ≥ 1.85, installed via [`rustup`](https://rustup.rs)** — not the distro
package. Zed compiles the dev extension to `wasm32-wasip2` itself, with the
`cargo` it finds on `PATH`, and gets that target by running `rustup target add`;
an apt/dnf `rustc` has no rustup to do that. Ubuntu 24.04, for instance, ships
1.75, which is too old on two counts — `Cargo.lock` is version 4 (needs cargo ≥
1.78) and `lsp-server 0.10` requires `edition2024` (needs 1.85):

```
error: failed to parse lock file ...
  lock file version 4 requires `-Znext-lockfile-bump`

error: feature `edition2024` is required
```

```sh
sudo apt remove rustc cargo   # otherwise /usr/bin/rustc shadows rustup's
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env" && rustc --version   # must be >= 1.85
```

There is no way around this while the extension is not in Zed's store: `install
dev extension` always compiles `src/lib.rs`. Building the *language server*,
on the other hand, is optional — see
[Testing without building the language server](#testing-without-building-the-language-server).

### Steps

1. Build the language server: `cargo build -p http_request_client_lsp`.
   To use the extension in **other** projects from this checkout, install it on
   `$PATH`: `cargo install --path lsp-server`. (Anyone installing the extension
   from Zed's store doesn't need this — the binary is downloaded from the
   release; see
   [How the language server binary is found](#how-the-language-server-binary-is-found).)
2. Enable Code Lens in Zed's `settings.json`: `"code_lens": "on"`.
3. Open Zed **from a terminal that has `cargo` on `PATH`** — Zed inherits the
   environment of whoever started it, and needs `cargo` to compile the dev
   extension.
4. `zed: install dev extension` (command palette) and select this folder.
5. Copy the `.env` (`cp .env.example .env`) and open `api.http` — syntax
   highlighting is applied and the `▶ Send request` button appears above each
   request.

Recommended: `"autosave": "on_focus_change"` in `settings.json`. It is only
needed on the fallback path (clients that don't handle `window/showDocument`),
where the result tab is opened via `workspace/applyEdit` and is born "dirty"
(unsaved): autosave makes it clean, and that is what allows subsequent responses
to be updated on disk without stealing focus from the editor.

### Testing without building the language server

Step 1 can be skipped by using the binary already published in the release,
which is the same one users get. Download the asset for the platform, unpack it
(the asset is a gzip of the raw binary) and make it executable:

```sh
mkdir -p ~/.local/bin
curl -L https://github.com/feapps/zed-api-client/releases/latest/download/http-request-client-lsp-linux-x86_64.gz \
  | gunzip > ~/.local/bin/http-request-client-lsp
chmod +x ~/.local/bin/http-request-client-lsp
```

Then point Zed's `settings.json` at it:

```json
{
  "lsp": {
    "http-request-client": {
      "binary": { "path": "/home/you/.local/bin/http-request-client-lsp" }
    }
  }
}
```

Putting the binary on `$PATH` is **not** enough here: when the folder open in
Zed is this repository, resolution stops at `target/debug/` (step 2 of
[How the language server binary is found](#how-the-language-server-binary-is-found))
and never consults `$PATH` — and `api.http`, the file used for testing, lives in
this repository. The `settings.json` path is step 1, so it wins over that
shortcut.

Rust is still needed to compile the extension itself; this only avoids
compiling the server. And because the URL above is `releases/latest`, the binary
is the newest published one, which may be ahead of the working tree.

### If installation fails with `failed to compile grammar 'http'`

Zed keeps a checkout of the grammar in `grammars/` (generated by Zed itself,
git-ignored) and **refuses to reuse it once `extension.toml`'s `repository` has
changed**, with this message:

```
grammar directory '.../grammars/http' already exists,
but is not a git clone of 'https://github.com/feapps/tree-sitter-http'
```

This affects anyone who installed the extension while
`[grammars.http] repository` pointed at a local path. Since `grammars/` is a
regenerable artifact, delete it and install again:

```sh
rm -rf grammars/
```

Zed clones the grammar from scratch on the next install. The same applies
whenever `repository` or `rev` changes.

### How the language server binary is found

The WASM extension ([`src/lib.rs`](./src/lib.rs)) resolves the binary in the
following order, from the most explicit source to the most automatic:

1. the path configured in Zed's `settings.json`:

   ```json
   {
     "lsp": {
       "http-request-client": {
         "binary": { "path": "/path/to/http-request-client-lsp" }
       }
     }
   }
   ```

2. `target/debug/http-request-client-lsp` from the extension's own repository,
   when that repository is the one open in Zed — this way the development cycle
   (`cargo build` → restart the language server) works without installing
   anything;
3. `$PATH`, via `worktree.which(...)` — the case when using the extension in
   other projects, after `cargo install --path lsp-server`;
4. the binary published in the repository's **GitHub Release**, downloaded
   automatically for the current platform — the path taken by anyone who
   installs the extension from Zed's store, with no need for Rust or `cargo` on
   the machine.

The download in step 4 uses the
`http-request-client-lsp-<os>-<arch>.gz` asset from the latest release (`os` ∈
`macos`/`linux`/`windows`, `arch` ∈ `aarch64`/`x86_64`/`x86`), published by the
[`.github/workflows/release.yml`](./.github/workflows/release.yml) workflow. The
binary is stored in a versioned directory, reused on subsequent runs, and older
versions are removed. Progress shows up in Zed's UI as language server
installation status.

If none of the four work, Zed shows the reason for the failure.

## The "Send request" feature

- A `▶ Send request  (name)` Code Lens appears on the HTTP method line of every
  request (e.g. above `POST {{HOST}}/v1/oauth/login`). The name comes from
  `# @name`, when present.
- On click, the request runs on a separate thread (the editor doesn't freeze) and
  progress shows up in three places, in order of reliability:
  1. **`# ⏳ Sending…` in the result panel**, immediately, in place of the
     previous response. This is the primary feedback;
  2. **Zed's status bar**, via `$/progress` (`window/workDoneProgress/create`
     + `begin`/`end`). Doesn't depend on layout or focus;
  3. **the Code Lens turns into `⏳ Sending…`** — when Zed asks for it.

  **The `⏳` on the Code Lens depends on the panel layout.** Zed only requests
  lenses for the *visible* buffers of each editor (`visible_buffers`), so:

  - the result panel **in a split next to** the `.http` → both editors are
    visible, Zed requests the lenses for both, and the button switches to `⏳`
    and back normally;
  - the result panel as a **tab in the same pane**, on top of the `.http` → the
    `.http` editor is hidden, Zed stops requesting its lenses, and the button
    stays frozen on `▶ Send request` even while the request is running.

  In the log the difference is plain: in the first case every refresh produces a
  `-> codeLens` for the `.http` **and** another for the result file; in the
  second, only for the result file. This can't be worked around from the server
  — hence the two other indicators above, and the lock below.
- **One request at a time, per line.** Clicking again while one is in flight
  doesn't fire a second one: the click is blocked on the server (`inflight`) and
  turns into a `⏳ <name> is already running` warning. The lock has to live there
  precisely because the button doesn't always get to switch to `⏳`. Requests on
  different lines can still run in parallel.
- When the server does manage to serve the `⏳`, it holds it for at least 400 ms
  (`MIN_LOADING`): Zed waits 50 ms of debounce + 30 ms before asking for the
  lenses back, and a new `codeLens/refresh` **replaces** the pending request
  instead of queueing it — so, on a request that takes a few ms, the refresh at
  the end was cancelling the one at the start. The wait only delays the button
  returning to normal: the response has already been written before it.
- The response is always written to disk; if the client doesn't have the buffer
  open, a `window/showDocument` (without stealing focus) reveals the tab. On the
  `applyEdit` fallback, the first response goes through the edit itself — on that
  path the buffer may still be "dirty", and Zed's watcher would ignore the write
  to disk.
- The request is resolved and executed by the native `lsp-server` (not by the
  WASM extension, which has no network access):
  - `{{NAME}}` variables are resolved from `@NAME = value` declarations in the
    file and from `.env` variables (`{{$dotenv NAME}}`). Resolution is
    recursive, so `@HOST = {{$dotenv HOST}}` works;
  - the `.env` is looked up starting from the folder of the `.http` file and
    walking up to the workspace root — the closest one wins, which allows one
    `.env` per environment (e.g. `.rest/prd/.env`);
  - chained references to previous responses are resolved from the response
    cache, which is **persisted** in `<private-dir>/responses.json` (`0600`), in
    three forms:
    - `{{name.response.body.path}}` — walks the JSON of the body
      (e.g. `{{login.response.body.json.key}}`);
    - `{{name.response.headers.Header}}` — the value of a response header
      (case-insensitive match, e.g. `{{login.response.headers.content-type}}`);
    - `{{name.response.status}}` — HTTP status code (e.g. `200`).

    This cache is **per environment**, the environment being the folder of the
    `.http` file — the same criterion used to find the `.env`. So a
    `# @name login` in `.rest/hml/api.http` and another in `.rest/prd/api.http`
    keep independent tokens: authenticating in one environment doesn't drop the
    other's session. Two `.http` files in the **same** folder still share
    responses, which allows splitting (for instance) `login.http` and
    `orders.http` without having to repeat the login. Consequence: moving a
    `.http` to another folder resets its chaining, because the environment
    changed.

    The cache is persisted because Zed **stops and starts the language server
    mid-session** (it kills it when the last `.http` closes, and it also restarts
    on its own with files open — you can see a `=== starting ===` in the log with
    no `didClose` before it). While the cache lived only in memory, that cycle —
    invisible to whoever is using the extension — wiped the token from
    `# @name oauthLogin`, and subsequent requests failed with "unresolved
    variables" with nothing on screen explaining why.

    **Closing a `.http` deletes its stored responses** (memory and disk), with
    three caveats that exist so that things still in use don't get deleted:

    - the environment is the *folder*, so it is only cleared when **no other open
      `.http`** shares it — closing `login.http` doesn't drop the session of the
      `orders.http` open next to it;
    - the cleanup waits 3 s (`CLOSE_GRACE`) and is cancelled if the file comes
      back (`didOpen`) or shows signs of life (a Code Lens request) within that
      window — this is the filter for Zed's spurious `didClose`;
    - a server restart **without** a `didClose` deletes nothing: only what was
      pending cleanup is discarded on shutdown (`flush_pending_clears`).

    Accepted consequence: closing every `.http` and reopening later requires
    logging in again. Responses larger than 512 KiB
    (`MAX_RESPONSE_ENTRY_BYTES`) stay in memory only — you can chain off them
    within the session, but they don't go to disk, so that a 2 MB listing doesn't
    stop a 700-byte token from being saved (which is what happened with a cap on
    the total alone: `responses not persisted` in the log and nothing was
    written). When no response is left, `responses.json` is removed.
  - file inclusion in the body (REST Client style):
    - `< path` inserts the raw file contents (path relative to the `.http`);
    - `<@ path` inserts the contents and resolves `{{...}}` inside them.

    For safety, reading is **confined to the workspace root**: the path is
    canonicalized (resolving `..` and symlinks) and refused if it escapes it.
    Without that, a `.http` from an untrusted source could include
    `~/.ssh/id_rsa` and send the contents to an arbitrary server with one click.
    Blocked inclusions are recorded in the log and the `< ...` line is kept
    literally in the body.
  - if any `{{...}}` is left unresolved, the request is **not** sent: the result
    lists the missing variables, instead of an obscure error from the HTTP
    client.
- The parser tolerates the common patterns of real-world files: query strings
  across multiple lines (lines starting with `?` or `&`), comments between
  headers, commented-out query parameters, and comments after the body (which
  don't end up in the body sent).
- With **several `.http` files open at once**, all of them show lenses — and the
  buttons keep working after editing the files. Four defenses on the server
  ensure that, because the client is the fragile part here:
  - the lenses are drawn by the *editor*, and it only looks up buffers that are
    already registered and visible in it. Two races make that lookup come up
    empty, with nothing to reschedule it afterwards: opening a second `.http`
    (the lookup arrives before the buffer is registered) and opening Zed with
    `.http` files already open (workspace restoration is asynchronous, and the
    lookup can happen before the editor exists — the server does answer the
    lenses, you can see it in the log, and the tab stays without buttons until
    it's closed and reopened). The server sends
    `workspace/codeLens/refresh` at 50 ms, 400 ms, 1.5 s and 4 s after every
    `didOpen` (`LENS_NUDGES_MS`) to cover both;
  - the lenses don't depend on `didOpen`/`didClose` bookkeeping: if a document's
    text isn't in memory, it is read from disk. One extra `didClose` (preview
    tabs, the same file in two panes) would leave the tab mute until reopened;
  - the click doesn't trust any single argument from the lens. The client
    **freezes the command's arguments** when it receives the lens and anchors
    only the on-screen position, and it doesn't swap them even after receiving
    the lenses again — measured: 104 new lenses delivered on every `didChange`
    and the next click still arrived with the old arguments. The symptom was the
    worst of all: the button simply had no effect, and only closing and
    reopening the `.http` fixed it. That's why the lens carries **four hints**,
    and `resolve_request` tries them from the most stable to the most fragile,
    because each one dies to a different kind of edit:

    | hint | survives | dies with |
    | --- | --- | --- |
    | `# @name` | any change to the URL | renaming the request |
    | identity (method + URL + name) | line shifts | any edit to the request's text |
    | line | edits *inside* the request | inserting/removing lines above |
    | method | — | corroborates the line |

    The first two versions got exactly this wrong. The first sent only the line
    (`request on line 1336 not found`, with the request at 1330). The
    second gave identity priority over the line — and then appending an
    `&page_size=100` to the multi-line query started invalidating the button
    forever: `request not found at ...:163
    (key=Some(10878242406106393856))`, with line 163 still **correct**. Name
    before identity before line solves both, and the method rules out the only
    real risk of falling back to the line (shifted lines would fire the wrong
    request — these are real API calls). With none of them matching, the server
    **warns** instead of staying mute, and the message says to close and reopen
    the file, because a refresh doesn't undo the freezing;
  - the server only asks for `codeLens/refresh` on an edit when the edit touched
    the **position or the name** of some request (`lens_signature`). The refresh
    is global: it invalidates the lenses of *every* buffer, but Zed only
    re-requests those of the editors it considers visible — a `.http` hidden
    behind the response tab, or in another pane, was left with no lenses at all
    until reopened. Asking for a refresh on every keystroke, as before, made
    `▶ Send request` disappear after some time of use; typing inside a JSON body
    now invalidates nothing.

  The log lives in
  `<private-dir>/http-request-client-lsp-<workspace-name>.log` — one per
  project, because Zed starts one language server per open project and with a
  fixed path the logs got mixed together. It records a
  `-> codeLens <uri>: N lens, M sending` line per lens request, useful to tell
  "the server didn't answer" from "the client didn't ask", and to see whether the
  `⏳` was ever served.
- The result is formatted as status line + headers + body (pretty-printed when
  JSON) and written to `<private-dir>/requests/<workspace-name>.http`:

  ```
  HTTP/1.1 200 OK
  Date: Wed, 22 Jul 2026 19:49:13 GMT
  Content-Type: application/json; charset=utf-8
  ...

  {
    "message": "Welcome to api application."
  }
  ```

  The buffer is updated on disk and Zed's file watcher reloads it — without
  stealing focus, which is what allows keeping it in a split next to the `.http`.
  When the client doesn't have the file open, it is revealed with
  `window/showDocument` (`takeFocus: false`), which is idempotent: it doesn't
  duplicate the tab, doesn't leave the buffer dirty, and reopens the tab if it
  was closed.

  Zed **does not implement** `window/showDocument` (measured 2026-08-03: it
  answers `-32601 Unrecognized method`, and doesn't even announce the
  capability), so what actually runs today is the fallback below; the preferred
  path is ready for when Zed does implement it. If the client refuses the
  `window/showDocument`, answers with an error **or doesn't answer** within 3 s,
  the server falls back to the previous mechanism — `workspace/applyEdit` with a
  `CreateFile`, once per session — and delivers that request's response through
  it as well. That path exists because `applyEdit` was, up to now, the only known
  way to make Zed open a tab at the language server's request; it has two
  downsides `showDocument` doesn't: Zed's spurious `didClose` (preview tab, the
  same file in two panes) forces a choice between **duplicating the tab** and
  **writing the response to an invisible file**, and the buffer is born dirty
  (hence the autosave recommendation).

  The file lives **outside the project**, named after the workspace — this way it
  doesn't pollute the repository and doesn't need a `.gitignore` entry, and two
  projects open at the same time don't fight over the same file. Being outside
  the worktree isn't a problem: I tested it, and Zed creates an invisible
  single-file worktree for it, registers the language server in it and watches
  the file normally — the write to disk produces `didChange` as before. Expected
  consequence of living in the temp directory: responses don't survive a reboot.

  The `<private-dir>` is
  `<temp>/http-request-client-<uid>/<workspace-name>-<workspace-root-hash>/`,
  with `0700` permissions (and files with `0600`). API responses usually carry
  tokens and sensitive data, and the temp directory is shared: with a fixed path
  and default permissions, any other user (or service) on the machine could
  **read** the responses, or plant a symlink on the predictable path to
  **redirect** the write. The parent directory is created in exclusive mode and,
  if it already exists, is only reused after checking that it is a directory (not
  a symlink), with `0700` and owned by our uid — otherwise the server falls back
  to a random name. Since nobody else traverses that directory, the workspace
  subdirectory can have a predictable name. URLs recorded in the log also go
  **without the query string**, which is where tokens tend to travel.

  The path is **stable**: it depends on the user and the workspace root, not on
  the process. It used to be random per process, and that was a bug — since Zed
  kills and starts the language server throughout the session, each cycle
  debuted a new result path, Zed opened **yet another response tab** and the old
  ones were orphaned (that's how dozens of `/tmp/http-request-client-*` showed up
  over an afternoon of use).

### Request timeout

Every request has a duration cap, which applies to the **whole operation**
(DNS + connection + send + reading the response), not per stage. The default is
30 s.

It can be changed in two places, in this order of precedence:

| where | scope | example |
| --- | --- | --- |
| `# @timeout <seconds>` | only the request it sits on | `# @timeout 120` |
| `HTTP_REQUEST_TIMEOUT` in the `.env` | every request of the environment | `HTTP_REQUEST_TIMEOUT=120` |
| *(nothing)* | default | 30 s |

`0` in either one means **no limit**. This is the same convention as VS Code
REST Client's `rest-client.timeoutinmilliseconds` — which ships with `0` by
default, and that's why a slow request "works in VS Code" and blows up here:
over there nobody gives up waiting.

The directive goes on a comment line **above the method line**, together with
`# @name`:

```http
# @name clientsIndex
# @timeout 120
GET {{HOST}}/v1/clients
    ?from=2024-05-14
    &to=2026-08-03
###
```

The `.env` is the same one looked up for `{{$dotenv ...}}` — from the folder of
the `.http` up to the workspace root — so the cap can also be per environment: a
larger value in `.rest/prd/.env` than in `.rest/local/.env`. A non-numeric value
is ignored (with a log entry) and resolution falls through to the next level.

The log of each send says **which of the three** applied, which separates "the
default caught you" from "the number I picked wasn't enough":

```
2026-08-04T15:10:38.257-03:00 => GET http://127.0.0.1:8799/slow (timeout 2s, from # @timeout)
2026-08-04T15:10:40.338-03:00 request error after 2.0s: timeout: global
```

**Blowing the timeout does not cancel the server's work.** The client merely
stops waiting; whatever already started on the other side runs to completion.
That's why the result of a timeout isn't just the raw error from the HTTP client:

```
# Timeout after 30.0s

GET https://example/v1/resource

Limit: 30s (set by default)

The client stopped waiting, but the server may still be working on this
request — a timeout here does not cancel anything on the other side.

Clicking "Send request" again does not replace that work: it starts another
request on top of the one still running, which usually makes both slower.
Prefer waiting, or narrowing the request.

To allow more time:

  - this request only:  # @timeout 120   (seconds, on a line above it)
  - whole workspace:    HTTP_REQUEST_TIMEOUT=120   (in .env)

Use 0 in either place to wait with no limit.
```

The warning is there because the intuitive reading of a timeout is wrong and
costs investigation time: re-clicking after a timeout **stacks** another run on
top of the previous one, which is still running on the server. The symptom of
that — every subsequent attempt timing out too, even when asking for less data —
is indistinguishable from "the connection got stuck on the first request". It
isn't: each request builds its own `ureq::Agent`, with its own pool and TCP
connection, and when it's the double-click lock that kicks in it says so up front
(`⏳ <name> is already running`, and `click ignored, already running` in the
log).

Worth remembering that reducing the *page size* doesn't necessarily reduce the
server's work — if the API orders by a column different from the one it filters
on, the `LIMIT` comes after the sort and the database scans the whole range
anyway. A timeout that doesn't budge with `page_size=1` is usually this, not the
client.

### Server log

Lives in `<private-dir>/http-request-client-lsp-<workspace>.log`, truncated when
it grows past 2 MiB (`MAX_LOG_BYTES`). URLs go without the query string
(`url_no_query`), which is where tokens tend to travel.

Each line starts with the local time in the same format as `Zed.log` (RFC 3339
with offset, plus milliseconds). The format matches on purpose: it's what allows
opening the two side by side and matching "I clicked here" with "the server did
that" — without it, you can tell *what* happened, but not *when* nor in what
order, and no overlap between concurrent requests shows up.

Each line is assembled whole and written in a **single** `write` call in
`O_APPEND`. A `writeln!` with formatting arguments emits one `write` per piece of
the formatting, and since each request runs on its own thread the lines came out
interleaved in the file (`=> GET /x<- response (id ...)`) — precisely in the
concurrent stretches, which are the ones that matter most when investigating.

### Known limitation (syntax highlighting)

The `tree-sitter-http` grammar recognizes interpolations in the
`{{identifier}}` form (no space) — it covers cases like
`{{login.response.body.json.key}}` well. Processor variables with an argument and
a space, such as `{{$dotenv HOST}}`, are not recognized as a `variable` node and
therefore don't get the brace/identifier highlighting; the text is still correct
and functional, it just isn't colored as a variable. Fixing this would require
extending the grammar.

**Comments right after the body break the highlighting for the rest of the
file.** The `tree-sitter-http` grammar (even in its latest version) accepts
comments **before** the headers, but doesn't handle comments **after the body** of
a request, before the next `###`. Since the grammar's error recovery is weak, a
single case like that produces an error node that propagates and "erases" the
colors of everything that follows in the file. An example that breaks:

```http
# @name updateExample
PUT {{HOST}}/put
content-type: {{CONTENT_TYPE}}

{
  "name": "new name",
  "active": true
}

# active: enables/disables the record   <- comment after the body: breaks highlighting
###
```

Workaround: put documentation comments **before** the body (next to the headers,
where the grammar accepts them):

```http
# @name updateExample
# active: enables/disables the record   <- comment before the body: OK
PUT {{HOST}}/put
content-type: {{CONTENT_TYPE}}

{
  "name": "new name",
  "active": true
}
###
```

This affects only **syntax highlighting**; request execution (`Send request`),
parsing and variable resolution work normally even with comments after the body.
A definitive fix would require extending the comment rule and the grammar's error
recovery — the fork in [`grammars-src/`](./grammars-src/README.md) is already the
place for that (it's where the commented-out query params case was solved).

## Credits

- Grammar: [rest-nvim/tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http) (MIT),
  used through the fork in [`grammars-src/`](./grammars-src/README.md)
