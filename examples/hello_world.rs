use copypwasmta::{ClipboardContext, ClipboardProvider};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut ctx = ClipboardContext::new().unwrap();

    let msg = "Hello, world!";
    ctx.set_contents(msg.to_owned()).await.unwrap();

    let content = ctx.get_contents().await.unwrap();

    println!("{}", content);
}
