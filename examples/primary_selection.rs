#[cfg(target_os = "linux")]
use copypwasmta::x11_clipboard::{Primary, X11ClipboardContext};
#[cfg(target_os = "linux")]
use copypwasmta::ClipboardProvider;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
fn main() {
    let mut ctx = X11ClipboardContext::<Primary>::new().unwrap();

    let the_string = "Hello, world!";

    ctx.set_contents(the_string.to_owned()).await.unwrap();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Primary selection is only available under linux!");
}
