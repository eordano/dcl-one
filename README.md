# copypwasmta

copypwasmta is a [copypasta](https://github.com/alacritty/copypasta) fork, adding browser support for wasm / wasm-bindgen.

copypasta is a cross-platform library for getting and setting the contents of the OS-level clipboard.

The apis have been changed to async to support the browser interface.

## Example

```rust
use copypwasmta::{ClipboardContext, ClipboardProvider};

async fn example() {
    let mut ctx = ClipboardContext::new().unwrap();

    let msg = "Hello, world!";
    ctx.set_contents(msg.to_owned()).await.unwrap();

    let content = ctx.get_contents().await.unwrap();

    println!("{}", content);
}
```

## API

The `ClipboardProvider` trait has the following functions:

```rust
async fn get_contents(&mut self) -> Result<String, Box<Error>>;
async fn set_contents(&mut self, String) -> Result<(), Box<Error>>;
```

`ClipboardContext` is a type alias for one of {`WindowsClipboardContext`, `OSXClipboardContext`, `X11ClipboardContext`, `WasmClipboard`, `NopClipboardContext`}, all of which implement `ClipboardProvider`. Which concrete type is chosen for `ClipboardContext` depends on the OS (via conditional compilation).

On wasm, `set_external_clipboard` routes clipboard access through functions the embedder supplies, for contexts with no `window` (e.g. a web worker, which has no `navigator.clipboard`).

## License

`copypwasmta` is dual-licensed under MIT and Apache2.
