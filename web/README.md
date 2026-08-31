# Local browser interface

The interface Refinery's daemon serves at its loopback address, compiled into
the binary as an embedded asset table. There is no build step and no `src`
directory beside `static`: these three files *are* the sources, and the daemon
serves exactly these bytes.

| File               | Role                                             |
| ------------------ | ------------------------------------------------ |
| `static/index.html`| The shell, and every `<template>` the page clones |
| `static/app.css`   | Tokens and layout, light and dark                 |
| `static/app.js`    | Routing, transport, and rendering                 |

Embedding is a table in [`src/api/assets.rs`](../src/api/assets.rs) rather than
a directory walk, so a stray file in this folder can never end up inside a
distributed binary.

## The two rules the page is held to

**Nothing is rendered by assigning markup.** Every dynamic node is cloned from a
`<template>` in `index.html` and filled with `textContent`. Question labels,
refined prompts, transcript messages, and delivery errors are all text somebody
else wrote — a model, a caller, an upstream service — and the reliable way to
keep that text from becoming markup is to have no code path that could turn it
into markup. `src/api/assets/accessibility.rs` fails the build if `innerHTML`,
`insertAdjacentHTML`, `document.write`, or `eval` appears in the script.

**Forms are labelled where a check can see it.** Because the answer form lives
in templates rather than in strings, the same file asserts that every control
has a programmatic label, every button and link has an accessible name, every
`for` and `aria-*` reference resolves, grouped choices sit in a `fieldset` with
a `legend`, tables are captioned with scoped headers, and each view is labelled
by exactly one first-level heading.

## Authentication

`/v1` routes need the local bearer token. The page sends it in an
`Authorization` header, except on the event stream: `EventSource` cannot set
headers, so the daemon also accepts the token as a `token` query parameter.
`refinery open` uses that form to hand the token to a fresh browser, and the
page moves it into `sessionStorage` and rewrites the address bar on load so it
does not linger in history.

The shell itself is served without a token. It holds no case data, and refusing
it would only stop the page that asks for a token from rendering.

## Working on it

Edit a file and rebuild; `include_bytes!` picks the change up. To see it against
real data, run `refinery serve` and `refinery open`.

```sh
just interface-check   # parse the script (needs node; skipped without it)
cargo test api::assets # the accessibility and embedding checks
```
